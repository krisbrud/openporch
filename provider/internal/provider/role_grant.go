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
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// roleGrant is a member's role on an application (`openporch_application_user`)
// or an environment type (`openporch_environment_type_user`).
type roleGrant struct {
	client *client.Client
	// scope is the API path segment: "apps" or "env-types".
	scope string
}

var _ resource.ResourceWithImportState = &roleGrant{}

func newApplicationUser() resource.Resource     { return &roleGrant{scope: "apps"} }
func newEnvironmentTypeUser() resource.Resource { return &roleGrant{scope: "env-types"} }

// roleGrantModel holds either resource's attributes; Object is `app_id` or
// `env_type_id`.
type roleGrantModel struct {
	Object, UserID, Role types.String
}

type appUserModel struct {
	AppID  types.String `tfsdk:"app_id"`
	UserID types.String `tfsdk:"user_id"`
	Role   types.String `tfsdk:"role"`
}

type envTypeUserModel struct {
	EnvTypeID types.String `tfsdk:"env_type_id"`
	UserID    types.String `tfsdk:"user_id"`
	Role      types.String `tfsdk:"role"`
}

func (r *roleGrant) apps() bool { return r.scope == "apps" }

func (r *roleGrant) objectAttribute() string {
	if r.apps() {
		return "app_id"
	}
	return "env_type_id"
}

func (r *roleGrant) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	if r.apps() {
		resp.TypeName = req.ProviderTypeName + "_application_user"
	} else {
		resp.TypeName = req.ProviderTypeName + "_environment_type_user"
	}
}

func (r *roleGrant) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	if r.apps() {
		resp.Schema = schema.Schema{
			MarkdownDescription: "A member's role on an application: `viewer`, `developer` or `owner`. " +
				"Granting a role needs `owner` on the application and authority over the role. The " +
				"application's creator is already its owner. Import it as `APP/USER`.",
			Attributes: map[string]schema.Attribute{
				"app_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
					MarkdownDescription: "The application."},
				"user_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
					MarkdownDescription: "The member, a person or a service user."},
				"role": schema.StringAttribute{Required: true,
					Validators:          []validator.String{oneOf("viewer", "developer", "owner")},
					MarkdownDescription: "The role. Changing it changes the grant in place."},
			},
		}
		return
	}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A member's role on an environment type. The only role is `deployer`, which " +
			"lets the member deploy to environments of the type, create and delete them, and change " +
			"their values. Only administrators grant it. Import it as `TYPE/USER`.",
		Attributes: map[string]schema.Attribute{
			"env_type_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The environment type."},
			"user_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The member, a person or a service user."},
			"role": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString("deployer"),
				PlanModifiers: replace, Validators: []validator.String{oneOf("deployer")},
				MarkdownDescription: "The role, `deployer`."},
		},
	}
}

func (r *roleGrant) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

// get reads the model of either resource.
func (r *roleGrant) get(ctx context.Context, get func(context.Context, any) diag.Diagnostics) (roleGrantModel, diag.Diagnostics) {
	if r.apps() {
		var m appUserModel
		d := get(ctx, &m)
		return roleGrantModel{Object: m.AppID, UserID: m.UserID, Role: m.Role}, d
	}
	var m envTypeUserModel
	d := get(ctx, &m)
	return roleGrantModel{Object: m.EnvTypeID, UserID: m.UserID, Role: m.Role}, d
}

// value is the model of either resource, for setting state.
func (r *roleGrant) value(object string, g *client.RoleGrant) any {
	if r.apps() {
		return appUserModel{AppID: types.StringValue(object), UserID: types.StringValue(g.User), Role: types.StringValue(g.Role)}
	}
	return envTypeUserModel{EnvTypeID: types.StringValue(object), UserID: types.StringValue(g.User), Role: types.StringValue(g.Role)}
}

func (r *roleGrant) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	plan, d := r.get(ctx, req.Plan.Get)
	resp.Diagnostics.Append(d...)
	if resp.Diagnostics.HasError() {
		return
	}
	g, err := r.client.GrantRole(ctx, r.scope, plan.Object.ValueString(), plan.UserID.ValueString(), plan.Role.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "granting the role", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, r.value(plan.Object.ValueString(), g))...)
}

func (r *roleGrant) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	state, d := r.get(ctx, req.State.Get)
	resp.Diagnostics.Append(d...)
	if resp.Diagnostics.HasError() {
		return
	}
	g, err := r.client.Role(ctx, r.scope, state.Object.ValueString(), state.UserID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the role", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, r.value(state.Object.ValueString(), g))...)
}

// Update changes an application role; environment type roles are replaced.
func (r *roleGrant) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	plan, d := r.get(ctx, req.Plan.Get)
	resp.Diagnostics.Append(d...)
	if resp.Diagnostics.HasError() {
		return
	}
	if !r.apps() {
		resp.Diagnostics.AddError("Unexpected update", "An environment type role cannot be changed in place.")
		return
	}
	g, err := r.client.ChangeAppRole(ctx, plan.Object.ValueString(), plan.UserID.ValueString(), plan.Role.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "changing the role", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, r.value(plan.Object.ValueString(), g))...)
}

func (r *roleGrant) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	state, d := r.get(ctx, req.State.Get)
	resp.Diagnostics.Append(d...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.RevokeRole(ctx, r.scope, state.Object.ValueString(), state.UserID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "removing the role", err)
	}
}

// ImportState takes `APP/USER` or `TYPE/USER`.
func (r *roleGrant) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	form := "APP/USER"
	if !r.apps() {
		form = "TYPE/USER"
	}
	object, userID, ok := importSplit(req.ID, form, &resp.Diagnostics)
	if !ok {
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root(r.objectAttribute()), object)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("user_id"), userID)...)
}
