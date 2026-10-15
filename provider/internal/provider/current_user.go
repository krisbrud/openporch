package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// currentUser is the `openporch_current_user` data source: who the provider's
// token belongs to.
type currentUser struct {
	client *client.Client
}

func newCurrentUser() datasource.DataSource { return &currentUser{} }

type currentUserModel struct {
	ID   types.String `tfsdk:"id"`
	Type types.String `tfsdk:"type"`
	Name types.String `tfsdk:"name"`
	Role types.String `tfsdk:"role"`
}

func (d *currentUser) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_current_user"
}

func (d *currentUser) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "The user the provider's token belongs to, and their organization role.",
		Attributes: map[string]schema.Attribute{
			"id":   schema.StringAttribute{Computed: true, MarkdownDescription: "The user ID."},
			"type": schema.StringAttribute{Computed: true, MarkdownDescription: "`human` or `service`."},
			"name": schema.StringAttribute{Computed: true, MarkdownDescription: "The display name."},
			"role": schema.StringAttribute{Computed: true, MarkdownDescription: "The organization role, empty when the user has none."},
		},
	}
}

func (d *currentUser) Configure(_ context.Context, req datasource.ConfigureRequest, resp *datasource.ConfigureResponse) {
	if req.ProviderData == nil {
		return
	}
	c, ok := req.ProviderData.(*client.Client)
	if !ok {
		resp.Diagnostics.AddError("Unexpected provider data", "The openporch provider passed data of an unexpected type.")
		return
	}
	d.client = c
}

func (d *currentUser) Read(ctx context.Context, _ datasource.ReadRequest, resp *datasource.ReadResponse) {
	if d.client == nil {
		resp.Diagnostics.AddError("Provider not configured", "The openporch provider was not configured.")
		return
	}
	user, err := d.client.CurrentUser(ctx)
	if err != nil {
		resp.Diagnostics.AddError("Reading the current user failed", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, currentUserModel{
		ID:   types.StringValue(user.ID),
		Type: types.StringValue(user.Type),
		Name: types.StringValue(user.Name),
		Role: types.StringValue(user.Role),
	})...)
}
