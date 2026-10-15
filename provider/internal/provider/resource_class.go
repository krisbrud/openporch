package provider

import (
	"context"
	"fmt"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// resourceClass is the `openporch_resource_class` resource.
type resourceClass struct {
	client *client.Client
}

var _ resource.ResourceWithImportState = &resourceClass{}

func newResourceClass() resource.Resource { return &resourceClass{} }

type resourceClassModel struct {
	ResourceType types.String `tfsdk:"resource_type"`
	ID           types.String `tfsdk:"id"`
	Description  types.String `tfsdk:"description"`
}

func (r *resourceClass) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_resource_class"
}

func (r *resourceClass) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A class of a resource type, such as `large` for `postgres`. Workloads ask for a " +
			"class, and resource definitions match on it. Every type has the `default` class, which is not " +
			"managed here. A class cannot be changed, so any change replaces it. Import it as `TYPE/CLASS`.",
		Attributes: map[string]schema.Attribute{
			"resource_type": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The resource type, such as `postgres` or `acme/feature-flags`."},
			"id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The class ID."},
			"description": schema.StringAttribute{Optional: true, Computed: true, Default: stringdefault.StaticString(""),
				PlanModifiers: replace, MarkdownDescription: "A description."},
		},
	}
}

func (r *resourceClass) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func classModel(c *client.Class) resourceClassModel {
	return resourceClassModel{
		ResourceType: types.StringValue(c.ResourceType),
		ID:           types.StringValue(c.ID),
		Description:  types.StringValue(c.Description),
	}
}

func (r *resourceClass) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan resourceClassModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	c, err := r.client.CreateClass(ctx, plan.ResourceType.ValueString(), plan.ID.ValueString(), plan.Description.ValueString())
	if err != nil {
		apiError(&resp.Diagnostics, "creating the resource class", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, classModel(c))...)
}

func (r *resourceClass) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state resourceClassModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	c, err := r.client.Class(ctx, state.ResourceType.ValueString(), state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the resource class", err)
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, classModel(c))...)
}

// Update is never called: every attribute replaces the class.
func (r *resourceClass) Update(_ context.Context, _ resource.UpdateRequest, resp *resource.UpdateResponse) {
	resp.Diagnostics.AddError("Unexpected update", "A resource class cannot be changed in place.")
}

func (r *resourceClass) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state resourceClassModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteClass(ctx, state.ResourceType.ValueString(), state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the resource class", err)
	}
}

// ImportState takes `TYPE/CLASS`. Custom types contain a `/`, so the class is
// what follows the last one.
func (r *resourceClass) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	i := strings.LastIndex(req.ID, "/")
	if i <= 0 || i == len(req.ID)-1 {
		resp.Diagnostics.AddError("Invalid import ID", fmt.Sprintf("%q: expected TYPE/CLASS", req.ID))
		return
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("resource_type"), req.ID[:i])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("id"), req.ID[i+1:])...)
}
