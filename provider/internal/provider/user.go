package provider

import (
	"context"
	"fmt"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// user is the `openporch_user` resource, a service user.
type user struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &user{}
	_ resource.ResourceWithValidateConfig = &user{}
)

func newUser() resource.Resource { return &user{} }

type userModel struct {
	ID   types.String `tfsdk:"id"`
	Name types.String `tfsdk:"name"`
	Role types.String `tfsdk:"role"`
}

var orgRoles = []string{"administrator", "manager", "member", "orgViewer"}

func (r *user) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_user"
}

func (r *user) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "A service user, for CI and automation. People join through invitations and " +
			"are not managed here. Deleting a service user revokes its tokens and removes its roles. " +
			"Import it by ID.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
				MarkdownDescription: "The user ID. Changing it replaces the user."},
			"name": nameAttribute(),
			"role": schema.StringAttribute{Required: true, Validators: []validator.String{oneOf(orgRoles...)},
				MarkdownDescription: "The organization role: `administrator`, `manager`, `member` or " +
					"`orgViewer`. Granting a role needs authority over it."},
		},
	}
}

func (r *user) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *user) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config userModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	validateName(config.Name, &resp.Diagnostics)
}

func userState(u *client.User) userModel {
	return userModel{ID: types.StringValue(u.ID), Name: types.StringValue(u.Name), Role: types.StringValue(u.Role)}
}

func (r *user) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan userModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	u, err := r.client.CreateServiceUser(ctx, plan.ID.ValueString(), nameOrID(plan.Name, plan.ID), plan.Role.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "creating the service user", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, userState(u))...)
}

func (r *user) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state userModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	u, err := r.client.User(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the service user", err)
		return
	}
	if u.Type != "service" {
		resp.Diagnostics.AddError("Not a service user",
			fmt.Sprintf("`%s` is a person, who joins through an invitation; only service users are managed by this resource.", u.ID))
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, userState(u))...)
}

func (r *user) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state userModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	name, role := "", ""
	if !plan.Name.IsUnknown() && !plan.Name.Equal(state.Name) {
		name = plan.Name.ValueString()
	}
	if !plan.Role.Equal(state.Role) {
		role = plan.Role.ValueString()
	}
	u, err := r.client.UpdateUser(ctx, plan.ID.ValueString(), name, role)
	if err != nil {
		apiError(&resp.Diagnostics, "updating the service user", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, userState(u))...)
}

func (r *user) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state userModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteUser(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the service user", err)
	}
}

func (r *user) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}

// oneOf accepts only the given strings.
type oneOfValidator []string

func oneOf(values ...string) validator.String { return oneOfValidator(values) }

func (v oneOfValidator) Description(context.Context) string {
	return fmt.Sprintf("must be one of %q", []string(v))
}

func (v oneOfValidator) MarkdownDescription(ctx context.Context) string { return v.Description(ctx) }

func (v oneOfValidator) ValidateString(ctx context.Context, req validator.StringRequest, resp *validator.StringResponse) {
	if req.ConfigValue.IsNull() || req.ConfigValue.IsUnknown() {
		return
	}
	for _, ok := range v {
		if req.ConfigValue.ValueString() == ok {
			return
		}
	}
	resp.Diagnostics.AddAttributeError(req.Path, "Invalid value",
		fmt.Sprintf("%q %s.", req.ConfigValue.ValueString(), v.Description(ctx)))
}
