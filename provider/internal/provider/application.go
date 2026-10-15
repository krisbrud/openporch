package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// application is the `openporch_application` resource.
type application struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &application{}
	_ resource.ResourceWithModifyPlan     = &application{}
	_ resource.ResourceWithValidateConfig = &application{}
)

func newApplication() resource.Resource { return &application{} }

type applicationModel struct {
	ID          types.String `tfsdk:"id"`
	Name        types.String `tfsdk:"name"`
	DestroyAcks types.Set    `tfsdk:"destroy_acknowledgements"`
}

func (r *application) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_application"
}

func (r *application) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "An application. It is created with a `development` environment, and the provider's " +
			"user becomes its `owner`. Deleting it deletes every environment and the resources they own, " +
			"behind `destroy_acknowledgements`. Import it by ID.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "The application ID. Changing it replaces the application.",
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"name": schema.StringAttribute{
				Optional:            true,
				Computed:            true,
				MarkdownDescription: "The display name. Defaults to the ID.",
			},
			"destroy_acknowledgements": destroyAcksAttribute(true),
		},
	}
}

func (r *application) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *application) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config applicationModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	parseAcks(ctx, config.DestroyAcks, true, &resp.Diagnostics)
}

// ModifyPlan plans the default name, the ID, so the plan shows it.
func (r *application) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() {
		return
	}
	var config, plan applicationModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() || !config.Name.IsNull() {
		return
	}
	resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, path.Root("name"), plan.ID)...)
}

func (r *application) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan applicationModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	app, err := r.client.CreateApp(ctx, plan.ID.ValueString(), plan.Name.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "creating the application", err)
		return
	}
	plan.Name = types.StringValue(app.Name)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *application) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state applicationModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	app, err := r.client.App(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the application", err)
		return
	}
	state.ID = types.StringValue(app.ID)
	state.Name = types.StringValue(app.Name)
	if state.DestroyAcks.IsUnknown() {
		state.DestroyAcks = types.SetNull(types.StringType)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *application) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state applicationModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if !plan.Name.Equal(state.Name) {
		app, err := r.client.RenameApp(ctx, plan.ID.ValueString(), plan.Name.ValueString())
		if err != nil {
			apiError(&resp.Diagnostics, "renaming the application", err)
			return
		}
		plan.Name = types.StringValue(app.Name)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *application) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state applicationModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	acks := parseAcks(ctx, state.DestroyAcks, true, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.client.DeleteApp(ctx, state.ID.ValueString(), acks, deleteComment); err != nil {
		apiError(&resp.Diagnostics, "deleting the application", err)
	}
}

func (r *application) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
