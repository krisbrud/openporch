package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/objectplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// identityProvider is the `openporch_identity_provider` resource: an OpenID
// Connect provider people sign in with.
type identityProvider struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState = &identityProvider{}
	_ resource.ResourceWithModifyPlan  = &identityProvider{}
)

func newIdentityProvider() resource.Resource { return &identityProvider{} }

type identityProviderModel struct {
	ID             types.String `tfsdk:"id"`
	Issuer         types.String `tfsdk:"issuer"`
	ClientID       types.String `tfsdk:"client_id"`
	ClientSecret   types.String `tfsdk:"client_secret"`
	Written        types.Object `tfsdk:"written_client_secret_ref"`
	CallbackURL    types.String `tfsdk:"callback_url"`
	ReturnURLs     types.List   `tfsdk:"return_urls"`
	LoopbackReturn types.Bool   `tfsdk:"loopback_return"`
	Scopes         types.List   `tfsdk:"scopes"`
	Algorithms     types.List   `tfsdk:"algorithms"`
}

func stringList(values ...string) types.List {
	elems := make([]attr.Value, 0, len(values))
	for _, v := range values {
		elems = append(elems, types.StringValue(v))
	}
	return types.ListValueMust(types.StringType, elems)
}

func listStrings(l types.List) []string {
	if l.IsNull() || l.IsUnknown() {
		return nil
	}
	out := make([]string, 0, len(l.Elements()))
	for _, e := range l.Elements() {
		if s, ok := e.(types.String); ok {
			out = append(out, s.ValueString())
		}
	}
	return out
}

func (r *identityProvider) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_identity_provider"
}

func (r *identityProvider) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "An OpenID Connect provider people sign in with through " +
			"`/auth/oidc/<id>/authorize`, like one under `[[oidc_login]]` in the server's configuration " +
			"file, whose IDs cannot be used here. Changes apply to the next login without a restart. " +
			"A `client_secret` is written to the built-in secret store and kept in the OpenTofu state, so " +
			"protect the state. Deleting the provider stops sign-ins through it; members keep their " +
			"`<issuer>:<sub>` identity. Import it by ID; an imported client secret is written again by the " +
			"first apply that configures one. Needs a server with `/orgs/{org}/identity-providers`.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()},
				MarkdownDescription: "The provider ID in the login routes, a lowercase ID other than " +
					"`github`. Changing it replaces the provider."},
			"issuer": schema.StringAttribute{Required: true,
				MarkdownDescription: "The issuer, exactly as its discovery document and ID tokens name it. " +
					"Changing it changes the identities it signs in, `<issuer>:<sub>`."},
			"client_id": schema.StringAttribute{Required: true,
				MarkdownDescription: "The OAuth client ID registered with the provider."},
			"client_secret": schema.StringAttribute{Optional: true, Sensitive: true,
				MarkdownDescription: "The client secret, written to the built-in store. Left out, the " +
					"provider is a public client, which PKCE alone protects. Changing it writes a new " +
					"version; `written_client_secret_ref` records the version written, and a secret changed " +
					"outside OpenTofu plans a write of this one again."},
			"written_client_secret_ref": schema.SingleNestedAttribute{Computed: true,
				Attributes:          secretRefAttributes(true),
				PlanModifiers:       []planmodifier.Object{objectplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "Where the client secret was written."},
			"callback_url": schema.StringAttribute{Required: true,
				MarkdownDescription: "This server's `/auth/oidc/<id>/callback` URL, as registered with the " +
					"provider."},
			"return_urls": schema.ListAttribute{ElementType: types.StringType, Optional: true, Computed: true,
				Default:             listdefault.StaticValue(stringList()),
				MarkdownDescription: "The pages a finished login may return the browser to."},
			"loopback_return": schema.BoolAttribute{Optional: true, Computed: true,
				Default: booldefault.StaticBool(false),
				MarkdownDescription: "Also return a finished login to any port of the loopback address, as " +
					"`porch login --provider` listens on. Defaults to `false`."},
			"scopes": schema.ListAttribute{ElementType: types.StringType, Optional: true, Computed: true,
				Default:             listdefault.StaticValue(stringList("openid", "email", "profile")),
				MarkdownDescription: "The scopes requested, including `openid`."},
			"algorithms": schema.ListAttribute{ElementType: types.StringType, Optional: true, Computed: true,
				Default:             listdefault.StaticValue(stringList("RS256")),
				MarkdownDescription: "The ID token signing algorithms accepted."},
		},
	}
}

func (r *identityProvider) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

// ModifyPlan plans the written reference: none without a secret, which also
// shows a secret the server holds that the configuration leaves out, and a new
// one when the secret or the ID changes.
func (r *identityProvider) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || req.State.Raw.IsNull() {
		return
	}
	var plan, state identityProviderModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	written := path.Root("written_client_secret_ref")
	if plan.ClientSecret.IsNull() {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.ObjectNull(secretRefAttrTypes))...)
	} else if !plan.ClientSecret.Equal(state.ClientSecret) || !plan.ID.Equal(state.ID) {
		// A new ID replaces the provider, whose secret is written under the new ID.
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.ObjectUnknown(secretRefAttrTypes))...)
	}
}

// body is the write that makes the server's provider match m. A secret the
// provider wrote before and that is unchanged is sent as its reference, which
// keeps it.
func (m identityProviderModel) body(prior *identityProviderModel) client.IdentityProviderWrite {
	w := client.IdentityProviderWrite{
		ID:             m.ID.ValueString(),
		Issuer:         m.Issuer.ValueString(),
		ClientID:       m.ClientID.ValueString(),
		CallbackURL:    m.CallbackURL.ValueString(),
		ReturnURLs:     listStrings(m.ReturnURLs),
		LoopbackReturn: m.LoopbackReturn.ValueBool(),
		Scopes:         listStrings(m.Scopes),
		Algorithms:     listStrings(m.Algorithms),
	}
	if w.ReturnURLs == nil {
		w.ReturnURLs = []string{}
	}
	if !m.ClientSecret.IsNull() {
		if prior != nil && prior.ClientSecret.Equal(m.ClientSecret) {
			if ref := refFromObject(prior.Written); ref != nil {
				w.ClientSecret = &client.SecretWrite{SecretRef: ref}
				return w
			}
		}
		v := m.ClientSecret.ValueString()
		w.ClientSecret = &client.SecretWrite{Value: &v}
	}
	return w
}

// apply records the server's provider in m.
func (m *identityProviderModel) apply(p *client.IdentityProvider) diag.Diagnostics {
	var diags diag.Diagnostics
	m.ID = types.StringValue(p.ID)
	m.Issuer = types.StringValue(p.Issuer)
	m.ClientID = types.StringValue(p.ClientID)
	m.CallbackURL = types.StringValue(p.CallbackURL)
	m.ReturnURLs = stringList(p.ReturnURLs...)
	m.LoopbackReturn = types.BoolValue(p.LoopbackReturn)
	m.Scopes = stringList(p.Scopes...)
	m.Algorithms = stringList(p.Algorithms...)
	switch {
	case p.ClientSecret == nil:
		// A secret removed outside OpenTofu is forgotten, so the plan writes it again.
		m.ClientSecret = types.StringNull()
		m.Written = types.ObjectNull(secretRefAttrTypes)
	case !m.ClientSecret.IsNull():
		written := refFromObject(m.Written)
		if written != nil && *written != *p.ClientSecret {
			// Changed outside OpenTofu: forget it, so the plan writes it again.
			m.ClientSecret = types.StringNull()
		}
		m.Written = writtenRef(*p.ClientSecret)
	default:
		m.Written = writtenRef(*p.ClientSecret)
	}
	return diags
}

func (r *identityProvider) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan identityProviderModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, err := r.client.CreateIdentityProvider(ctx, plan.body(nil))
	if err != nil {
		apiError(&resp.Diagnostics, "creating the identity provider", err)
		return
	}
	plan.Written = types.ObjectNull(secretRefAttrTypes)
	resp.Diagnostics.Append(plan.apply(p)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *identityProvider) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state identityProviderModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, err := r.client.IdentityProvider(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the identity provider", err)
		return
	}
	if p.Source != "" && p.Source != "api" {
		resp.Diagnostics.AddError("Identity provider in the configuration file",
			"Identity provider `"+p.ID+"` is configured in the server's configuration file and cannot "+
				"be managed with OpenTofu.")
		return
	}
	resp.Diagnostics.Append(state.apply(p)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *identityProvider) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state identityProviderModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p, err := r.client.ReplaceIdentityProvider(ctx, plan.ID.ValueString(), plan.body(&state))
	if err != nil {
		apiError(&resp.Diagnostics, "updating the identity provider", err)
		return
	}
	// The server holds what was just written, so there is no drift to detect.
	plan.Written = types.ObjectNull(secretRefAttrTypes)
	resp.Diagnostics.Append(plan.apply(p)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *identityProvider) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state identityProviderModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteIdentityProvider(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the identity provider", err)
	}
}

func (r *identityProvider) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
