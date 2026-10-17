package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// environmentType is the `openporch_environment_type` resource.
type environmentType struct {
	client *client.Client
}

var _ resource.ResourceWithImportState = &environmentType{}

func newEnvironmentType() resource.Resource { return &environmentType{} }

type environmentTypeModel struct {
	ID          types.String `tfsdk:"id"`
	Description types.String `tfsdk:"description"`
	Protected   types.Bool   `tfsdk:"protected"`
}

func environmentTypeState(t *client.EnvType) environmentTypeModel {
	return environmentTypeModel{
		ID:          types.StringValue(t.ID),
		Description: types.StringValue(t.Description),
		Protected:   types.BoolValue(t.Protected),
	}
}

func (r *environmentType) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_environment_type"
}

func (r *environmentType) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "An environment type, such as `staging` or `production`. Resource definitions " +
			"match on it, and the `deployer` role is granted per type. It can only be deleted while no " +
			"environment uses it. Import it by ID.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "The type ID. Changing it replaces the type.",
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"description": schema.StringAttribute{
				Optional:            true,
				Computed:            true,
				Default:             stringdefault.StaticString(""),
				MarkdownDescription: "A description.",
			},
			"protected": schema.BoolAttribute{
				Optional: true,
				Computed: true,
				Default:  booldefault.StaticBool(false),
				MarkdownDescription: "Whether environments of this type are protected: a permit to delete, replace or " +
					"detach resources there needs an administrator, and a change whose impact its driver " +
					"cannot preview gets no permit. Only administrators change it. Defaults to `false`, so leaving it " +
					"out unprotects a type protected outside OpenTofu.",
			},
		},
	}
}

func (r *environmentType) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *environmentType) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan environmentTypeModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.CreateEnvType(ctx, plan.ID.ValueString(), plan.Description.ValueString(), plan.Protected.ValueBool())
	if err != nil {
		apiError(&resp.Diagnostics, "creating the environment type", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, environmentTypeState(t))...)
}

func (r *environmentType) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state environmentTypeModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.EnvType(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the environment type", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, environmentTypeState(t))...)
}

func (r *environmentType) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan environmentTypeModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.UpdateEnvType(ctx, plan.ID.ValueString(), plan.Description.ValueString(), plan.Protected.ValueBool())
	if err != nil {
		apiError(&resp.Diagnostics, "updating the environment type", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, environmentTypeState(t))...)
}

func (r *environmentType) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state environmentTypeModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteEnvType(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the environment type", err)
	}
}

func (r *environmentType) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
