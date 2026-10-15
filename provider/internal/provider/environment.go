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

// environment is the `openporch_environment` resource.
type environment struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &environment{}
	_ resource.ResourceWithValidateConfig = &environment{}
)

func newEnvironment() resource.Resource { return &environment{} }

type environmentModel struct {
	AppID       types.String `tfsdk:"app_id"`
	ID          types.String `tfsdk:"id"`
	Name        types.String `tfsdk:"name"`
	Type        types.String `tfsdk:"type"`
	DestroyAcks types.Set    `tfsdk:"destroy_acknowledgements"`
}

func (r *environment) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_environment"
}

func (r *environment) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "An environment of an application. Creating one needs `developer` on the " +
			"application and `deployer` on the type. Every application already has `development`, which " +
			"goes with the application and is not managed here. Deleting an environment deletes the " +
			"resources it owns, behind `destroy_acknowledgements`. Import it as `APP/ENV`.",
		Attributes: map[string]schema.Attribute{
			"app_id": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "The application. Changing it replaces the environment.",
				PlanModifiers:       replace,
			},
			"id": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "The environment ID. Changing it replaces the environment.",
				PlanModifiers:       replace,
			},
			"name": nameAttribute(),
			"type": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "The environment type. Changing it replaces the environment.",
				PlanModifiers:       replace,
			},
			"destroy_acknowledgements": destroyAcksAttribute(false),
		},
	}
}

func (r *environment) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *environment) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config environmentModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if config.ID.ValueString() == "development" {
		resp.Diagnostics.AddAttributeError(path.Root("id"), "The development environment is not managed here",
			"Every application has a `development` environment, created and deleted with the application.")
	}
	validateName(config.Name, &resp.Diagnostics)
	parseAcks(ctx, config.DestroyAcks, false, &resp.Diagnostics)
}

func (m *environmentModel) set(env *client.Env) {
	m.AppID = types.StringValue(env.App)
	m.ID = types.StringValue(env.ID)
	m.Name = types.StringValue(env.Name)
	m.Type = types.StringValue(env.Type)
}

func (r *environment) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan environmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	env, err := r.client.CreateEnv(ctx, plan.AppID.ValueString(), plan.ID.ValueString(),
		nameOrID(plan.Name, plan.ID), plan.Type.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "creating the environment", err)
		return
	}
	plan.set(env)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *environment) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state environmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	env, err := r.client.Env(ctx, state.AppID.ValueString(), state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the environment", err)
		return
	}
	state.set(env)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *environment) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state environmentModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if !plan.Name.Equal(state.Name) {
		env, err := r.client.RenameEnv(ctx, plan.AppID.ValueString(), plan.ID.ValueString(), plan.Name.ValueString())
		if err != nil {
			apiError(&resp.Diagnostics, "renaming the environment", err)
			return
		}
		plan.set(env)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *environment) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state environmentModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	acks := parseAcks(ctx, state.DestroyAcks, false, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteEnv(ctx, state.AppID.ValueString(), state.ID.ValueString(), acks[""], deleteComment)
	if err != nil {
		apiError(&resp.Diagnostics, "deleting the environment", err)
	}
}

func (r *environment) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	app, env, ok := importSplit(req.ID, "APP/ENV", &resp.Diagnostics)
	if !ok {
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("app_id"), app)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), env)...)
}
