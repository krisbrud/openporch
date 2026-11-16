package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// resourceType is the `openporch_resource_type` resource: a custom resource type.
type resourceType struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &resourceType{}
	_ resource.ResourceWithModifyPlan     = &resourceType{}
	_ resource.ResourceWithValidateConfig = &resourceType{}
)

func newResourceType() resource.Resource { return &resourceType{} }

type resourceTypeModel struct {
	ID            types.String  `tfsdk:"id"`
	Name          types.String  `tfsdk:"name"`
	Use           types.String  `tfsdk:"use"`
	Category      types.String  `tfsdk:"category"`
	InputsSchema  types.Dynamic `tfsdk:"inputs_schema"`
	OutputsSchema types.Dynamic `tfsdk:"outputs_schema"`
}

// The schemas the server gives a type that leaves them out. A schema left out
// of the configuration stays null in state while the server holds its default.
func defaultInputsSchema() map[string]any { return map[string]any{"type": "object"} }

func defaultOutputsSchema() map[string]any {
	return map[string]any{
		"type": "object",
		"properties": map[string]any{
			"values":  map[string]any{"type": "object"},
			"secrets": map[string]any{"type": "object"},
		},
	}
}

var resourceUses = []string{"direct", "indirect", "implicit"}

func (r *resourceType) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_resource_type"
}

func (r *resourceType) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "A custom resource type, such as `acme/feature-flags`. Workloads request it and " +
			"resource definitions provision it like a built-in type. Changes replace the type's body in place; " +
			"deployments already accepted keep checking outputs against the schemas they recorded. Built-in " +
			"types cannot be managed. A type that resource definitions use cannot be deleted, and deleting a " +
			"type deletes its classes. Import it by ID.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
				MarkdownDescription: "The type ID, `<org>/<name>`. Changing it replaces the type."},
			"name": schema.StringAttribute{Optional: true, Computed: true,
				MarkdownDescription: "The display name. Defaults to the ID."},
			"use": schema.StringAttribute{Required: true,
				MarkdownDescription: "How the type is used: `direct` (workloads request it), `indirect` (only " +
					"other resources reference it) or `implicit` (one per environment)."},
			"category": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString(""),
				MarkdownDescription: "A grouping for display, such as `datastore`."},
			"inputs_schema": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "JSON Schema of the resource inputs (a workload's `params`), an object. " +
					"Left out, any object is accepted."},
			"outputs_schema": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "JSON Schema of the outputs: an object whose `properties` are the `values` " +
					"and `secrets` schemas. Drivers' outputs are checked against it. Left out, any values and " +
					"secrets are accepted."},
		},
	}
}

func (r *resourceType) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *resourceType) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config resourceTypeModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() || config.Use.IsNull() || config.Use.IsUnknown() {
		return
	}
	for _, u := range resourceUses {
		if config.Use.ValueString() == u {
			return
		}
	}
	resp.Diagnostics.AddAttributeError(path.Root("use"), "Invalid use",
		"`use` is one of `direct`, `indirect` and `implicit`.")
}

// ModifyPlan plans the server's default name, the ID, when none is configured.
func (r *resourceType) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() {
		return
	}
	var config, plan resourceTypeModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !config.Name.IsNull() {
		return
	}
	resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, path.Root("name"), plan.ID)...)
}

func (m *resourceTypeModel) body(ctx context.Context) (client.ResourceTypeWrite, diag.Diagnostics) {
	var diags diag.Diagnostics
	w := client.ResourceTypeWrite{
		ID:       m.ID.ValueString(),
		Use:      m.Use.ValueString(),
		Category: m.Category.ValueString(),
	}
	if !m.Name.IsNull() && !m.Name.IsUnknown() {
		name := m.Name.ValueString()
		w.Name = &name
	}
	for _, s := range []struct {
		name string
		from types.Dynamic
		to   *any
	}{{"inputs_schema", m.InputsSchema, &w.InputsSchema}, {"outputs_schema", m.OutputsSchema, &w.OutputsSchema}} {
		v, err := dynamicToJSON(ctx, s.from)
		if err != nil {
			diags.AddAttributeError(path.Root(s.name), "Invalid schema", err.Error())
			continue
		}
		if v != nil {
			*s.to = v
		}
	}
	return w, diags
}

// apply records the server's type in m. A schema keeps its configured form while
// the server holds the same JSON, and stays null while the server holds the
// default.
func (m *resourceTypeModel) apply(ctx context.Context, t *client.ResourceType) diag.Diagnostics {
	var diags diag.Diagnostics
	m.ID = types.StringValue(t.ID)
	m.Name = types.StringValue(t.Name)
	m.Use = types.StringValue(t.Use)
	m.Category = types.StringValue(t.Category)
	for _, s := range []struct {
		name     string
		field    *types.Dynamic
		server   map[string]any
		fallback map[string]any
	}{
		{"inputs_schema", &m.InputsSchema, t.InputsSchema, defaultInputsSchema()},
		{"outputs_schema", &m.OutputsSchema, t.OutputsSchema, defaultOutputsSchema()},
	} {
		current, err := dynamicToJSON(ctx, *s.field)
		switch {
		case err == nil && current == nil && (s.server == nil || sameJSON(s.server, s.fallback)):
			*s.field = types.DynamicNull()
		case err == nil && current != nil && sameJSON(current, s.server):
		default:
			v, err := jsonToDynamic(ctx, s.server)
			if err != nil {
				diags.AddError("Unexpected resource type "+s.name, err.Error())
				return diags
			}
			*s.field = v
		}
	}
	return diags
}

func (r *resourceType) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan resourceTypeModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body, diags := plan.body(ctx)
	resp.Diagnostics.Append(diags...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.CreateType(ctx, body)
	if err != nil {
		apiError(&resp.Diagnostics, "creating the resource type", err)
		return
	}
	resp.Diagnostics.Append(plan.apply(ctx, t)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *resourceType) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state resourceTypeModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.Type(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the resource type", err)
		return
	}
	resp.Diagnostics.Append(state.apply(ctx, t)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *resourceType) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan resourceTypeModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body, diags := plan.body(ctx)
	resp.Diagnostics.Append(diags...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.ReplaceType(ctx, plan.ID.ValueString(), body)
	if err != nil {
		apiError(&resp.Diagnostics, "updating the resource type", err)
		return
	}
	resp.Diagnostics.Append(plan.apply(ctx, t)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *resourceType) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state resourceTypeModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteType(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the resource type", err)
	}
}

func (r *resourceType) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
