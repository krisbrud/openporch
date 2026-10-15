package provider

import (
	"context"
	"fmt"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/mapplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// cloudAccount is the `openporch_cloud_account` resource.
type cloudAccount struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &cloudAccount{}
	_ resource.ResourceWithModifyPlan     = &cloudAccount{}
	_ resource.ResourceWithValidateConfig = &cloudAccount{}
)

func newCloudAccount() resource.Resource { return &cloudAccount{} }

type cloudAccountModel struct {
	ID         types.String  `tfsdk:"id"`
	Name       types.String  `tfsdk:"name"`
	Type       types.String  `tfsdk:"type"`
	Config     types.Dynamic `tfsdk:"config"`
	Secrets    types.Map     `tfsdk:"secrets"`
	SecretRefs types.Map     `tfsdk:"secret_refs"`
	Written    types.Map     `tfsdk:"written_secret_refs"`
}

func (r *cloudAccount) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_cloud_account"
}

func (r *cloudAccount) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A cloud account: the credentials openporch uses to reach a cloud. Resource " +
			"definitions name it in `driver_account`. Secrets set in `secrets` are written to the built-in " +
			"secret store and kept in the OpenTofu state, so protect the state; see the provider " +
			"documentation. An account that definitions or active resources use cannot be deleted. " +
			"Import it by ID; secrets are imported as references.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The account ID. Changing it replaces the account."},
			"name": nameAttribute(),
			"type": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The account type: `aws` (a static key pair) or `aws-role` (a role " +
					"openporch assumes). Changing it replaces the account."},
			"config": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "The non-secret configuration, an object: `access_key_id` and `region` " +
					"for `aws`; `role_arn`, `external_id`, `session_duration` (seconds) and `region` for `aws-role`."},
			"secrets": schema.MapAttribute{ElementType: types.StringType, Optional: true, Sensitive: true,
				MarkdownDescription: "Secret configuration, such as `secret_access_key` for `aws`, written to " +
					"the built-in store. Changing an entry writes a new version. The server never returns " +
					"the values; `written_secret_refs` records the versions written, and a version changed " +
					"outside OpenTofu plans a write of the entry again."},
			"secret_refs": schema.MapNestedAttribute{Optional: true,
				NestedObject: schema.NestedAttributeObject{Attributes: secretRefAttributes(false)},
				MarkdownDescription: "Secret configuration as references to secrets already in a store. " +
					"A key cannot be in both `secrets` and `secret_refs`."},
			"written_secret_refs": schema.MapNestedAttribute{Computed: true,
				NestedObject:        schema.NestedAttributeObject{Attributes: secretRefAttributes(true)},
				PlanModifiers:       []planmodifier.Map{mapplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "Where each entry of `secrets` was written."},
		},
	}
}

func (r *cloudAccount) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *cloudAccount) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config cloudAccountModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	validateName(config.Name, &resp.Diagnostics)
	if config.Secrets.IsUnknown() || config.SecretRefs.IsUnknown() {
		return
	}
	refs := config.SecretRefs.Elements()
	for key := range config.Secrets.Elements() {
		if _, ok := refs[key]; ok {
			resp.Diagnostics.AddAttributeError(path.Root("secret_refs").AtMapKey(key), "Duplicate secret",
				fmt.Sprintf("Secret `%s` is set in both `secrets` and `secret_refs`.", key))
		}
	}
}

// ModifyPlan marks the written references unknown when secrets are written.
func (r *cloudAccount) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || req.State.Raw.IsNull() {
		return
	}
	var plan, state cloudAccountModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	written := path.Root("written_secret_refs")
	if plan.Secrets.IsNull() {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.MapNull(secretRefType))...)
	} else if !plan.Secrets.Equal(state.Secrets) {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.MapUnknown(secretRefType))...)
	}
}

func stringMap(m types.Map) map[string]string {
	out := map[string]string{}
	for k, v := range m.Elements() {
		if s, ok := v.(types.String); ok && !s.IsNull() && !s.IsUnknown() {
			out[k] = s.ValueString()
		}
	}
	return out
}

func refMap(m types.Map) map[string]client.SecretRef {
	out := map[string]client.SecretRef{}
	for k, v := range m.Elements() {
		if o, ok := v.(types.Object); ok {
			if ref := refFromObject(o); ref != nil {
				out[k] = *ref
			}
		}
	}
	return out
}

// body is the write that makes the server's account match m. A secret the
// provider wrote before and that is unchanged is sent as its reference, so it
// keeps its version.
func (m cloudAccountModel) body(ctx context.Context, prior *cloudAccountModel, name string) (client.AccountWrite, error) {
	w := client.AccountWrite{
		ID: m.ID.ValueString(), Name: name, Type: m.Type.ValueString(),
		Config: map[string]any{}, SecretRefs: map[string]client.SecretWrite{},
	}
	config, err := dynamicToJSON(ctx, m.Config)
	if err != nil {
		return w, fmt.Errorf("config: %w", err)
	}
	if config != nil {
		obj, ok := config.(map[string]any)
		if !ok {
			return w, fmt.Errorf("config: expected an object")
		}
		w.Config = obj
	}
	var priorSecrets map[string]string
	var priorWritten map[string]client.SecretRef
	if prior != nil {
		priorSecrets, priorWritten = stringMap(prior.Secrets), refMap(prior.Written)
	}
	for key, value := range stringMap(m.Secrets) {
		if old, ok := priorSecrets[key]; ok && old == value {
			if ref, ok := priorWritten[key]; ok {
				w.SecretRefs[key] = client.SecretWrite{SecretRef: &ref}
				continue
			}
		}
		v := value
		w.SecretRefs[key] = client.SecretWrite{Value: &v}
	}
	for key, ref := range refMap(m.SecretRefs) {
		w.SecretRefs[key] = client.SecretWrite{SecretRef: &ref}
	}
	return w, nil
}

// apply records the server's account in m.
func (m *cloudAccountModel) apply(ctx context.Context, a *client.Account) diag.Diagnostics {
	var diags diag.Diagnostics
	m.ID, m.Name, m.Type = types.StringValue(a.ID), types.StringValue(a.Name), types.StringValue(a.Type)

	current, err := dynamicToJSON(ctx, m.Config)
	switch {
	case err == nil && current == nil && len(a.Config) == 0:
	case err == nil && sameJSON(current, map[string]any(a.Config)):
	default:
		config := map[string]any(a.Config)
		if config == nil {
			config = map[string]any{}
		}
		d, err := jsonToDynamic(ctx, config)
		if err != nil {
			diags.AddError("Unexpected account configuration", err.Error())
			return diags
		}
		m.Config = d
	}

	// Secrets the provider wrote stay in `secrets` while the server still has the
	// version written; one changed outside OpenTofu is forgotten, so the plan
	// writes it again.
	configured := stringMap(m.Secrets)
	priorWritten := refMap(m.Written)
	kept := map[string]attr.Value{}
	written := map[string]attr.Value{}
	for key, value := range configured {
		ref, ok := a.SecretRefs[key]
		if !ok {
			continue
		}
		if old, ok := priorWritten[key]; ok && old != ref {
			continue
		}
		kept[key] = types.StringValue(value)
		written[key] = writtenRef(ref)
	}
	if m.Secrets.IsNull() {
		m.Written = types.MapNull(secretRefType)
	} else {
		m.Secrets = types.MapValueMust(types.StringType, kept)
		m.Written = types.MapValueMust(secretRefType, written)
	}

	refs := map[string]attr.Value{}
	for key, ref := range a.SecretRefs {
		if _, ok := configured[key]; !ok {
			refs[key] = readRef(ref)
		}
	}
	if len(refs) > 0 || !m.SecretRefs.IsNull() {
		m.SecretRefs = types.MapValueMust(secretRefType, refs)
	}
	return diags
}

func (r *cloudAccount) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan cloudAccountModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body, err := plan.body(ctx, nil, nameOrID(plan.Name, plan.ID))
	if err != nil {
		resp.Diagnostics.AddError("Invalid cloud account", err.Error())
		return
	}
	a, err := r.client.CreateAccount(ctx, body)
	if err != nil {
		apiError(&resp.Diagnostics, "creating the cloud account", err)
		return
	}
	plan.Written = types.MapNull(secretRefType)
	resp.Diagnostics.Append(plan.apply(ctx, a)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *cloudAccount) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state cloudAccountModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	a, err := r.client.Account(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the cloud account", err)
		return
	}
	resp.Diagnostics.Append(state.apply(ctx, a)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *cloudAccount) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state cloudAccountModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	name := plan.Name.ValueString()
	if plan.Name.IsUnknown() || plan.Name.IsNull() {
		name = state.Name.ValueString()
	}
	body, err := plan.body(ctx, &state, name)
	if err != nil {
		resp.Diagnostics.AddError("Invalid cloud account", err.Error())
		return
	}
	a, err := r.client.ReplaceAccount(ctx, plan.ID.ValueString(), body)
	if err != nil {
		apiError(&resp.Diagnostics, "updating the cloud account", err)
		return
	}
	// Only the references of secrets written before are compared for drift.
	plan.Written = types.MapNull(secretRefType)
	resp.Diagnostics.Append(plan.apply(ctx, a)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *cloudAccount) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state cloudAccountModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteAccount(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the cloud account", err)
	}
}

func (r *cloudAccount) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
