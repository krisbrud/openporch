package provider

import (
	"context"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

var secretRefAttrTypes = map[string]attr.Type{
	"store":   types.StringType,
	"ref":     types.StringType,
	"version": types.StringType,
}

var secretRefType = types.ObjectType{AttrTypes: secretRefAttrTypes}

// secretRefAttributes are a reference's attributes, configured or computed.
func secretRefAttributes(computed bool) map[string]schema.Attribute {
	if computed {
		return map[string]schema.Attribute{
			"store":   schema.StringAttribute{Computed: true, MarkdownDescription: "The secret store."},
			"ref":     schema.StringAttribute{Computed: true, MarkdownDescription: "The secret's path in the store."},
			"version": schema.StringAttribute{Computed: true, MarkdownDescription: "The version written."},
		}
	}
	return map[string]schema.Attribute{
		"store": schema.StringAttribute{Required: true, MarkdownDescription: "The secret store, such as `openporch`."},
		"ref":   schema.StringAttribute{Required: true, MarkdownDescription: "The secret's path in the store."},
		"version": schema.StringAttribute{Optional: true, Validators: []validator.String{notEmpty{}},
			MarkdownDescription: "A version; without one, deployments pin the version current when they are accepted."},
	}
}

// readRef is a configured reference as the server returns it; a reference
// written without a version comes back without one.
func readRef(r client.SecretRef) types.Object {
	version := types.StringValue(r.Version)
	if r.Version == "" {
		version = types.StringNull()
	}
	return types.ObjectValueMust(secretRefAttrTypes, map[string]attr.Value{
		"store":   types.StringValue(r.Store),
		"ref":     types.StringValue(r.Ref),
		"version": version,
	})
}

// writtenRef is the computed reference of a secret the provider wrote.
func writtenRef(r client.SecretRef) types.Object {
	return types.ObjectValueMust(secretRefAttrTypes, map[string]attr.Value{
		"store":   types.StringValue(r.Store),
		"ref":     types.StringValue(r.Ref),
		"version": types.StringValue(r.Version),
	})
}

// refFromObject reads a reference object, or nil when it is unset or unknown.
func refFromObject(o types.Object) *client.SecretRef {
	if o.IsNull() || o.IsUnknown() {
		return nil
	}
	a := o.Attributes()
	str := func(k string) string {
		if s, ok := a[k].(types.String); ok {
			return s.ValueString()
		}
		return ""
	}
	return &client.SecretRef{Store: str("store"), Ref: str("ref"), Version: str("version")}
}

// notEmpty rejects an empty string, which the API would read as "no version".
type notEmpty struct{}

func (notEmpty) Description(context.Context) string { return "must not be empty" }

func (v notEmpty) MarkdownDescription(ctx context.Context) string { return v.Description(ctx) }

func (notEmpty) ValidateString(_ context.Context, req validator.StringRequest, resp *validator.StringResponse) {
	if !req.ConfigValue.IsNull() && !req.ConfigValue.IsUnknown() && req.ConfigValue.ValueString() == "" {
		resp.Diagnostics.AddAttributeError(req.Path, "Empty version",
			"Leave `version` out for the current version, or give one.")
	}
}
