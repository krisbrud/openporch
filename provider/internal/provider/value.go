package provider

import (
	"context"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/objectplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// value is the `openporch_value` resource.
type value struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &value{}
	_ resource.ResourceWithModifyPlan     = &value{}
	_ resource.ResourceWithValidateConfig = &value{}
)

func newValue() resource.Resource { return &value{} }

type valueModel struct {
	AppID       types.String `tfsdk:"app_id"`
	EnvID       types.String `tfsdk:"env_id"`
	Key         types.String `tfsdk:"key"`
	Description types.String `tfsdk:"description"`
	Value       types.String `tfsdk:"value"`
	SecretValue types.String `tfsdk:"secret_value"`
	SecretRef   types.Object `tfsdk:"secret_ref"`
	Written     types.Object `tfsdk:"written_secret_ref"`
}

func (r *value) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_value"
}

func (r *value) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A shared value of an application, or of one of its environments, which overrides " +
			"the application's. Workloads read it as `${values.KEY}`. It holds exactly one of a plain `value`, " +
			"a `secret_value` written to the built-in secret store, or a `secret_ref` to a secret already in a " +
			"store. A `secret_value` is kept in the OpenTofu state, so protect the state; see the provider " +
			"documentation. Import it as `APP/KEY` or `APP/ENV/KEY`.",
		Attributes: map[string]schema.Attribute{
			"app_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The application."},
			"env_id": schema.StringAttribute{Optional: true, PlanModifiers: replace,
				MarkdownDescription: "The environment, for an environment value. The application must have a value with the same key."},
			"key": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The key: letters, digits, `_` and `-`, starting with a letter or `_`."},
			"description": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString(""),
				MarkdownDescription: "A description."},
			"value": schema.StringAttribute{Optional: true, MarkdownDescription: "A plain value."},
			"secret_value": schema.StringAttribute{Optional: true, Sensitive: true,
				MarkdownDescription: "A secret, written to the built-in store. Changing it writes a new version. " +
					"The server never returns it; `written_secret_ref` records the version written, and a version " +
					"changed outside OpenTofu plans a write of this value again."},
			"secret_ref": schema.SingleNestedAttribute{Optional: true, Attributes: secretRefAttributes(false),
				MarkdownDescription: "A reference to a secret in a store. Referencing a secret openporch did not " +
					"write needs a secret-use grant."},
			"written_secret_ref": schema.SingleNestedAttribute{Computed: true, Attributes: secretRefAttributes(true),
				MarkdownDescription: "Where `secret_value` was written.",
				PlanModifiers:       []planmodifier.Object{objectplanmodifier.UseStateForUnknown()}},
		},
	}
}

func (r *value) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *value) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config valueModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	set := 0
	for _, unset := range []bool{config.Value.IsNull(), config.SecretValue.IsNull(), config.SecretRef.IsNull()} {
		if !unset {
			set++
		}
	}
	if set != 1 {
		resp.Diagnostics.AddError("Invalid value",
			"Set exactly one of `value`, `secret_value` and `secret_ref`.")
	}
}

// ModifyPlan marks the written reference unknown when a new secret is written.
func (r *value) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || req.State.Raw.IsNull() {
		return
	}
	var plan, state valueModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if plan.SecretValue.IsNull() {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, path.Root("written_secret_ref"), types.ObjectNull(secretRefAttrTypes))...)
	} else if !plan.SecretValue.Equal(state.SecretValue) {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, path.Root("written_secret_ref"), types.ObjectUnknown(secretRefAttrTypes))...)
	}
}

// body is the write that makes the server's value match m. A secret the
// provider wrote before is sent as its reference, so an unchanged secret keeps
// its version.
func (m valueModel) body(prior *valueModel) client.ValueWrite {
	w := client.ValueWrite{Key: m.Key.ValueString(), Description: m.Description.ValueString()}
	switch {
	case !m.Value.IsNull():
		v := m.Value.ValueString()
		w.Value = &v
	case !m.SecretRef.IsNull():
		w.SecretRef = &client.SecretWrite{SecretRef: refFromObject(m.SecretRef)}
	default:
		if prior != nil && prior.SecretValue.Equal(m.SecretValue) {
			if ref := refFromObject(prior.Written); ref != nil {
				w.SecretRef = &client.SecretWrite{SecretRef: ref}
				return w
			}
		}
		v := m.SecretValue.ValueString()
		w.SecretRef = &client.SecretWrite{Value: &v}
	}
	return w
}

// apply records the server's value in m.
func (m *valueModel) apply(v *client.SharedValue) {
	m.Key = types.StringValue(v.Key)
	m.Description = types.StringValue(v.Description)
	switch {
	case v.SecretRef == nil:
		m.Value = types.StringPointerValue(v.Value)
		m.SecretValue, m.SecretRef = types.StringNull(), types.ObjectNull(secretRefAttrTypes)
		m.Written = types.ObjectNull(secretRefAttrTypes)
	case !m.SecretValue.IsNull():
		written := refFromObject(m.Written)
		if written != nil && *written != *v.SecretRef {
			// Changed outside OpenTofu: forget the value, so the plan writes it again.
			m.SecretValue = types.StringNull()
		}
		m.Written = writtenRef(*v.SecretRef)
	default:
		m.Value, m.SecretRef = types.StringNull(), readRef(*v.SecretRef)
		m.Written = types.ObjectNull(secretRefAttrTypes)
	}
}

func (r *value) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan valueModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	v, err := r.client.CreateValue(ctx, plan.AppID.ValueString(), plan.EnvID.ValueString(), plan.body(nil))
	if err != nil {
		apiError(&resp.Diagnostics, "creating the value", err)
		return
	}
	plan.Written = types.ObjectNull(secretRefAttrTypes)
	plan.apply(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *value) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state valueModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	v, err := r.client.Value(ctx, state.AppID.ValueString(), state.EnvID.ValueString(), state.Key.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the value", err)
		return
	}
	state.apply(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *value) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state valueModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	v, err := r.client.ReplaceValue(ctx, plan.AppID.ValueString(), plan.EnvID.ValueString(),
		plan.Key.ValueString(), plan.body(&state))
	if err != nil {
		apiError(&resp.Diagnostics, "updating the value", err)
		return
	}
	plan.Written = state.Written
	if !plan.SecretValue.Equal(state.SecretValue) {
		plan.Written = types.ObjectNull(secretRefAttrTypes)
	}
	plan.apply(v)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *value) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state valueModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteValue(ctx, state.AppID.ValueString(), state.EnvID.ValueString(), state.Key.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the value", err)
	}
}

// ImportState takes `APP/KEY` or `APP/ENV/KEY`. An imported secret value is
// imported as its reference, since its value cannot be read.
func (r *value) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts := strings.Split(req.ID, "/")
	for _, p := range parts {
		if p == "" {
			parts = nil
		}
	}
	switch len(parts) {
	case 2:
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("app_id"), parts[0])...)
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("key"), parts[1])...)
	case 3:
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("app_id"), parts[0])...)
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("env_id"), parts[1])...)
		resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("key"), parts[2])...)
	default:
		resp.Diagnostics.AddError("Invalid import ID", req.ID+": expected APP/KEY or APP/ENV/KEY")
	}
}
