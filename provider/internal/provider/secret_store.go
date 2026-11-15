package provider

import (
	"context"
	"fmt"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// secretStore is the `openporch_secret_store` resource: an external secret store.
type secretStore struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &secretStore{}
	_ resource.ResourceWithValidateConfig = &secretStore{}
)

func newSecretStore() resource.Resource { return &secretStore{} }

type secretStoreModel struct {
	ID           types.String `tfsdk:"id"`
	Type         types.String `tfsdk:"type"`
	Region       types.String `tfsdk:"region"`
	CloudAccount types.String `tfsdk:"cloud_account"`
	Primary      types.Bool   `tfsdk:"primary"`
}

var secretStoreTypes = []string{"aws-secrets-manager", "aws-ssm"}

func (r *secretStore) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secret_store"
}

func (r *secretStore) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "An external secret store: AWS Secrets Manager or SSM Parameter Store, read and " +
			"written with a cloud account. References name it in `store`. The region and account change in " +
			"place; changing either makes openporch forget which secrets it created there. A store that is " +
			"primary, or that anything still references, cannot be deleted. The built-in `openporch` store " +
			"always exists and is not managed here. Import it by ID.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The store ID. Changing it replaces the store."},
			"type": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "`aws-secrets-manager` (a reference names a secret, a version is its " +
					"`VersionId`) or `aws-ssm` (a reference names a parameter, a version is its number). " +
					"Changing it replaces the store."},
			"region": schema.StringAttribute{Required: true,
				MarkdownDescription: "The AWS region, such as `eu-north-1`."},
			"cloud_account": schema.StringAttribute{Required: true,
				MarkdownDescription: "The cloud account that reads and writes the store."},
			"primary": schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "Whether new `{value}` secrets and drivers' secret outputs are written to " +
					"this store. Setting it first opens the store with its account, so the change fails while " +
					"AWS refuses the account's credentials. `false` does not unmark the store: a store stays " +
					"primary until another store is marked, so switch by setting `primary` on the other store " +
					"(or marking the built-in store with `PATCH /orgs/{org}/secretstores/openporch`). A store " +
					"set to `true` that lost the mark is marked again."},
		},
	}
}

func (r *secretStore) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *secretStore) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config secretStoreModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() || config.Type.IsNull() || config.Type.IsUnknown() {
		return
	}
	for _, t := range secretStoreTypes {
		if config.Type.ValueString() == t {
			return
		}
	}
	resp.Diagnostics.AddAttributeError(path.Root("type"), "Invalid store type",
		fmt.Sprintf("`type` is one of %s.", strings.Join(secretStoreTypes, ", ")))
}

func (m *secretStoreModel) config() map[string]string {
	return map[string]string{"region": m.Region.ValueString(), "cloud_account": m.CloudAccount.ValueString()}
}

// apply records the server's store in m. A store configured as primary records
// whether it still is; one configured as not primary stays false, since it
// does not ask to be unmarked.
func (m *secretStoreModel) apply(s *client.SecretStore) {
	m.ID = types.StringValue(s.ID)
	m.Type = types.StringValue(s.Type)
	m.Region = types.StringValue(s.Config["region"])
	m.CloudAccount = types.StringValue(s.Config["cloud_account"])
	if m.Primary.ValueBool() || m.Primary.IsNull() || m.Primary.IsUnknown() {
		m.Primary = types.BoolValue(s.Primary)
	}
}

func (r *secretStore) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan secretStoreModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	s, err := r.client.CreateSecretStore(ctx, plan.ID.ValueString(), plan.Type.ValueString(), plan.config())
	if err != nil {
		apiError(&resp.Diagnostics, "registering the secret store", err)
		return
	}
	primary := plan.Primary.ValueBool()
	plan.Primary = types.BoolValue(false)
	plan.apply(s)
	if primary {
		// Keep the registered store in state if the promotion fails, so the
		// next plan retries only the promotion.
		resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
		s, err = r.client.UpdateSecretStore(ctx, plan.ID.ValueString(), nil, true)
		if err != nil {
			apiError(&resp.Diagnostics, "marking the secret store primary", err)
			return
		}
		plan.Primary = types.BoolValue(true)
		plan.apply(s)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *secretStore) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state secretStoreModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	s, err := r.client.SecretStore(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the secret store", err)
		return
	}
	state.apply(s)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *secretStore) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state secretStoreModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var config map[string]string
	if !plan.Region.Equal(state.Region) || !plan.CloudAccount.Equal(state.CloudAccount) {
		config = plan.config()
	}
	// The server opens the store as configured before it marks it, and changes
	// nothing if that fails.
	s, err := r.client.UpdateSecretStore(ctx, plan.ID.ValueString(), config, plan.Primary.ValueBool())
	if err != nil {
		apiError(&resp.Diagnostics, "updating the secret store", err)
		return
	}
	plan.apply(s)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *secretStore) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state secretStoreModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteSecretStore(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the secret store", err)
	}
}

// ImportState takes the store ID. An imported store that is primary is
// recorded as `primary = true`.
func (r *secretStore) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}

// secretGrant is the `openporch_secret_grant` resource: a secret-use grant.
type secretGrant struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &secretGrant{}
	_ resource.ResourceWithValidateConfig = &secretGrant{}
)

func newSecretGrant() resource.Resource { return &secretGrant{} }

type secretGrantModel struct {
	ID      types.String `tfsdk:"id"`
	Store   types.String `tfsdk:"store"`
	Ref     types.String `tfsdk:"ref"`
	AppID   types.String `tfsdk:"app_id"`
	EnvID   types.String `tfsdk:"env_id"`
	OrgWide types.Bool   `tfsdk:"org_wide"`
}

func (r *secretGrant) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_secret_grant"
}

func (r *secretGrant) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A secret-use grant: lets an application, one of its environments, or the whole " +
			"organization use one reference that it did not write. Holding a reference is not permission to " +
			"use it; deployments check grants right before they read a secret, so deleting a grant blocks every " +
			"later use, including replays. A grant cannot be changed, so any change replaces it. Import it as " +
			"`STORE/GRANT_ID`.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Computed: true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "The grant ID, chosen by the server."},
			"store": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The store, such as `openporch` or an `openporch_secret_store`'s ID."},
			"ref": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The reference in the store, without a version: a grant covers every version."},
			"app_id": schema.StringAttribute{Optional: true, PlanModifiers: replace,
				MarkdownDescription: "The application allowed to use the secret. Conflicts with `org_wide`."},
			"env_id": schema.StringAttribute{Optional: true, PlanModifiers: replace,
				MarkdownDescription: "Narrows the grant to one environment of `app_id`."},
			"org_wide": schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false),
				PlanModifiers: []planmodifier.Bool{boolplanmodifier.RequiresReplace()},
				MarkdownDescription: "Usable everywhere in the organization, meant for platform definitions. " +
					"Conflicts with `app_id`."},
		},
	}
}

func (r *secretGrant) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *secretGrant) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config secretGrantModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() || config.AppID.IsUnknown() || config.EnvID.IsUnknown() || config.OrgWide.IsUnknown() {
		return
	}
	app, env, org := !config.AppID.IsNull(), !config.EnvID.IsNull(), config.OrgWide.ValueBool()
	switch {
	case env && !app:
		resp.Diagnostics.AddAttributeError(path.Root("env_id"), "Missing application", "`env_id` needs `app_id`.")
	case app && org:
		resp.Diagnostics.AddAttributeError(path.Root("org_wide"), "Conflicting scope",
			"A grant is either org-wide or for an application, not both.")
	case !app && !org:
		resp.Diagnostics.AddError("Missing scope", "Set `app_id` (and optionally `env_id`) or `org_wide = true`.")
	}
}

func optionalString(s string) types.String {
	if s == "" {
		return types.StringNull()
	}
	return types.StringValue(s)
}

func grantModel(g *client.Grant) secretGrantModel {
	return secretGrantModel{
		ID:      types.StringValue(g.ID),
		Store:   types.StringValue(g.Store),
		Ref:     types.StringValue(g.Ref),
		AppID:   optionalString(g.AppID),
		EnvID:   optionalString(g.EnvID),
		OrgWide: types.BoolValue(g.OrgWide),
	}
}

func (r *secretGrant) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan secretGrantModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	g, err := r.client.CreateGrant(ctx, client.Grant{
		Store:   plan.Store.ValueString(),
		Ref:     plan.Ref.ValueString(),
		AppID:   plan.AppID.ValueString(),
		EnvID:   plan.EnvID.ValueString(),
		OrgWide: plan.OrgWide.ValueBool(),
	})
	if err != nil {
		apiError(&resp.Diagnostics, "creating the secret grant", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, grantModel(g))...)
}

func (r *secretGrant) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state secretGrantModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	g, err := r.client.Grant(ctx, state.Store.ValueString(), state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the secret grant", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, grantModel(g))...)
}

// Update is never called: every attribute replaces the grant.
func (r *secretGrant) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("Unexpected update", "A secret grant cannot be changed in place.")
}

func (r *secretGrant) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state secretGrantModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteGrant(ctx, state.Store.ValueString(), state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the secret grant", err)
	}
}

// ImportState takes `STORE/GRANT_ID`. Store IDs have no `/`.
func (r *secretGrant) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	store, id, ok := strings.Cut(req.ID, "/")
	if !ok || store == "" || id == "" {
		resp.Diagnostics.AddError("Invalid import ID", fmt.Sprintf("%q: expected STORE/GRANT_ID", req.ID))
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("store"), store)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), id)...)
}
