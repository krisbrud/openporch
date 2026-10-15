package provider

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// serviceUserToken is the `openporch_service_user_token` resource.
type serviceUserToken struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &serviceUserToken{}
	_ resource.ResourceWithValidateConfig = &serviceUserToken{}
	_ resource.ResourceWithModifyPlan     = &serviceUserToken{}
)

func newServiceUserToken() resource.Resource { return &serviceUserToken{} }

type tokenModel struct {
	UserID      types.String `tfsdk:"user_id"`
	ID          types.String `tfsdk:"id"`
	Description types.String `tfsdk:"description"`
	ExpiresAt   types.String `tfsdk:"expires_at"`
	Token       types.String `tfsdk:"token"`
}

func (r *serviceUserToken) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_service_user_token"
}

func (r *serviceUserToken) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "An API token of a service user. The server returns the token only when it " +
			"is created, so it is kept in the OpenTofu state, which must be protected; see the provider " +
			"documentation. A token cannot be changed: changing its description or expiry revokes it and issues a new one, " +
			"which needs a new `id`. " +
			"A token revoked outside OpenTofu leaves the state; issue it again under a new `id`, since a " +
			"revoked token keeps its ID. Import it as `USER/TOKEN`; an imported " +
			"token has no `token`.",
		Attributes: map[string]schema.Attribute{
			"user_id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The service user."},
			"id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The token ID, unique per user."},
			"description": schema.StringAttribute{Optional: true, PlanModifiers: replace,
				Validators: []validator.String{notEmpty{}}, MarkdownDescription: "A description."},
			"expires_at": schema.StringAttribute{Optional: true, PlanModifiers: replace,
				MarkdownDescription: "When the token expires, as an RFC 3339 time such as " +
					"`2027-01-01T00:00:00Z`. Without it, the token does not expire."},
			"token": schema.StringAttribute{Computed: true, Sensitive: true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "The token value."},
		},
	}
}

func (r *serviceUserToken) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func parseExpiry(s types.String) (*int64, error) {
	if s.IsNull() || s.IsUnknown() {
		return nil, nil
	}
	t, err := time.Parse(time.RFC3339, s.ValueString())
	if err != nil {
		return nil, fmt.Errorf("%q is not an RFC 3339 time, such as 2027-01-01T00:00:00Z", s.ValueString())
	}
	ms := t.UnixMilli()
	return &ms, nil
}

func (r *serviceUserToken) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config tokenModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if _, err := parseExpiry(config.ExpiresAt); err != nil {
		resp.Diagnostics.AddAttributeError(path.Root("expires_at"), "Invalid expiry", err.Error())
	}
}

// ModifyPlan refuses to replace a token under the same ID: the server keeps a
// revoked token's ID, so the new token could not be created.
func (r *serviceUserToken) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || req.State.Raw.IsNull() {
		return
	}
	var plan, state tokenModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() || plan.ID.IsUnknown() || plan.UserID.IsUnknown() {
		return
	}
	if !plan.ID.Equal(state.ID) || !plan.UserID.Equal(state.UserID) {
		return
	}
	planned, _ := parseExpiry(plan.ExpiresAt)
	current, _ := parseExpiry(state.ExpiresAt)
	sameExpiry := (planned == nil) == (current == nil) && (planned == nil || *planned == *current)
	if !plan.Description.Equal(state.Description) || !sameExpiry || plan.ExpiresAt.IsUnknown() {
		resp.Diagnostics.AddAttributeError(path.Root("id"), "Token needs a new ID",
			"Changing a token's description or expiry issues a new token, and a revoked token keeps its "+
				"ID, so give the token a new `id` in the same change.")
	}
}

// apply records the server's token in m, keeping a configured expiry that names
// the same instant.
func (m *tokenModel) apply(t *client.Token) {
	m.ID = types.StringValue(t.ID)
	m.Description = types.StringPointerValue(t.Description)
	current, err := parseExpiry(m.ExpiresAt)
	switch {
	case t.ExpiresAt == nil:
		m.ExpiresAt = types.StringNull()
	case err == nil && current != nil && *current == *t.ExpiresAt:
	default:
		m.ExpiresAt = types.StringValue(time.UnixMilli(*t.ExpiresAt).UTC().Format(time.RFC3339Nano))
	}
}

func (r *serviceUserToken) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan tokenModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	expires, err := parseExpiry(plan.ExpiresAt)
	if err != nil {
		resp.Diagnostics.AddAttributeError(path.Root("expires_at"), "Invalid expiry", err.Error())
		return
	}
	t, err := r.client.CreateToken(ctx, plan.UserID.ValueString(), plan.ID.ValueString(),
		plan.Description.ValueStringPointer(), expires)
	var apiErr *client.Error
	if errors.As(err, &apiErr) && apiErr.Status == http.StatusConflict {
		resp.Diagnostics.AddAttributeError(path.Root("id"), "Token ID already used",
			fmt.Sprintf("User `%s` already has a token `%s`. A revoked token keeps its ID, so a token revoked "+
				"outside OpenTofu needs a new `id`.", plan.UserID.ValueString(), plan.ID.ValueString()))
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "creating the token", err)
		return
	}
	plan.apply(&t.Token)
	plan.Token = types.StringValue(t.Value)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *serviceUserToken) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state tokenModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	t, err := r.client.Token(ctx, state.UserID.ValueString(), state.ID.ValueString())
	if client.IsNotFound(err) || (err == nil && t.RevokedAt != nil) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the token", err)
		return
	}
	state.apply(t)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

// Update is never called: every configured attribute replaces the token.
func (r *serviceUserToken) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("Unexpected update", "A token cannot be changed in place.")
}

func (r *serviceUserToken) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state tokenModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.RevokeToken(ctx, state.UserID.ValueString(), state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "revoking the token", err)
	}
}

func (r *serviceUserToken) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	userID, id, ok := importSplit(req.ID, "USER/TOKEN", &resp.Diagnostics)
	if !ok {
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("user_id"), userID)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), id)...)
}
