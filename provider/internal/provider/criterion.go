package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// definitionCriterion is the `openporch_resource_definition_criteria` resource.
type definitionCriterion struct {
	client *client.Client
}

var _ resource.ResourceWithImportState = &definitionCriterion{}

func newDefinitionCriterion() resource.Resource { return &definitionCriterion{} }

type criterionModel struct {
	DefinitionID types.String `tfsdk:"resource_definition_id"`
	ID           types.String `tfsdk:"id"`
	EnvType      types.String `tfsdk:"env_type"`
	AppID        types.String `tfsdk:"app_id"`
	EnvID        types.String `tfsdk:"env_id"`
	ResID        types.String `tfsdk:"res_id"`
	Class        types.String `tfsdk:"class"`
}

func (r *definitionCriterion) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_resource_definition_criteria"
}

func (r *definitionCriterion) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	match := func(what string) schema.StringAttribute {
		return schema.StringAttribute{Optional: true, PlanModifiers: replace,
			Validators: []validator.String{notEmpty{}}, MarkdownDescription: "Matches " + what + "; unset matches any."}
	}
	resp.Schema = schema.Schema{
		MarkdownDescription: "One matching criterion of a resource definition. A definition matches a " +
			"resource when any of its criteria matches, and a criterion matches when every attribute it " +
			"sets matches; one that sets nothing matches every resource of the `default` class. A " +
			"criterion cannot be changed, so any change replaces it. Replacing the definition (by changing " +
			"its `type`) deletes its criteria; give each criterion " +
			"`lifecycle { replace_triggered_by = [openporch_resource_definition.NAME] }` to recreate them " +
			"in the same apply. Import it as `DEFINITION/CRITERION`.",
		Attributes: map[string]schema.Attribute{
			"resource_definition_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The definition."},
			"id": schema.StringAttribute{Computed: true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "The criterion ID the server generates."},
			"env_type": match("the environment type"),
			"app_id":   match("the application"),
			"env_id":   match("the environment ID"),
			"res_id":   match("the resource ID, such as `modules.web.externals.db`"),
			"class":    match("the resource class; unset matches only `default`"),
		},
	}
}

func (r *definitionCriterion) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func optional(s string) types.String {
	if s == "" {
		return types.StringNull()
	}
	return types.StringValue(s)
}

func criterionState(def string, c *client.Criterion) criterionModel {
	return criterionModel{
		DefinitionID: types.StringValue(def), ID: types.StringValue(c.ID),
		EnvType: optional(c.EnvType), AppID: optional(c.AppID), EnvID: optional(c.EnvID),
		ResID: optional(c.ResID), Class: optional(c.Class),
	}
}

func (r *definitionCriterion) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan criterionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	def := plan.DefinitionID.ValueString()
	c, err := r.client.AddCriterion(ctx, def, client.Criterion{
		EnvType: plan.EnvType.ValueString(), AppID: plan.AppID.ValueString(), EnvID: plan.EnvID.ValueString(),
		ResID: plan.ResID.ValueString(), Class: plan.Class.ValueString(),
	})
	if err != nil {
		apiError(&resp.Diagnostics, "adding the criterion", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, criterionState(def, c))...)
}

func (r *definitionCriterion) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state criterionModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	def := state.DefinitionID.ValueString()
	c, err := r.client.Criterion(ctx, def, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the criterion", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, criterionState(def, c))...)
}

// Update is never called: every attribute replaces the criterion.
func (r *definitionCriterion) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("Unexpected update", "A criterion cannot be changed in place.")
}

func (r *definitionCriterion) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state criterionModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteCriterion(ctx, state.DefinitionID.ValueString(), state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "removing the criterion", err)
	}
}

func (r *definitionCriterion) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	def, id, ok := importSplit(req.ID, "DEFINITION/CRITERION", &resp.Diagnostics)
	if !ok {
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("resource_definition_id"), def)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), id)...)
}
