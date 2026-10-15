package provider

import (
	"context"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// deleteComment is the comment the provider gives its deletions.
const deleteComment = "deleted by the openporch OpenTofu provider"

// configureClient takes the client the provider configured.
func configureClient(req resource.ConfigureRequest, resp *resource.ConfigureResponse) *client.Client {
	if req.ProviderData == nil {
		return nil
	}
	c, ok := req.ProviderData.(*client.Client)
	if !ok {
		resp.Diagnostics.AddError("Unexpected provider data", "The openporch provider passed data of an unexpected type.")
		return nil
	}
	return c
}

// apiError adds err as an error diagnostic about doing what.
func apiError(diags *diag.Diagnostics, what string, err error) {
	diags.AddError("Failed "+what, err.Error())
}

// destroyAcksAttribute is the `destroy_acknowledgements` attribute; prefixed says
// whether entries name their environment, as `ENV:DESCRIPTOR=ACTION`.
func destroyAcksAttribute(prefixed bool) schema.SetAttribute {
	form := "`DESCRIPTOR=ACTION`"
	if prefixed {
		form = "`ENV:DESCRIPTOR=ACTION`"
	}
	return schema.SetAttribute{
		ElementType: types.StringType,
		Optional:    true,
		MarkdownDescription: "Acknowledges the resources that deleting this object deletes, each written " + form +
			" (with `@FINGERPRINT` when the preview shows one). Deleting it previews the deletion first and " +
			"deletes nothing unless every resource the preview marks is acknowledged; the error lists the " +
			"entries to add. Set them, apply, and then remove the object (if it is already removed from the " +
			"configuration, put it back with the entries first). Entries for changes the preview does " +
			"not show are ignored.",
	}
}

// parseAcks reads `destroy_acknowledgements`. With prefixed, entries are
// `ENV:DESCRIPTOR=ACTION` and the result is keyed by environment; without, the
// key is "".
func parseAcks(ctx context.Context, set types.Set, prefixed bool, diags *diag.Diagnostics) map[string][]client.Ack {
	out := map[string][]client.Ack{}
	if set.IsNull() || set.IsUnknown() {
		return out
	}
	var entries []string
	diags.Append(set.ElementsAs(ctx, &entries, false)...)
	for _, entry := range entries {
		env, rest := "", entry
		if prefixed {
			var ok bool
			env, rest, ok = strings.Cut(entry, ":")
			if !ok || env == "" {
				diags.AddAttributeError(path.Root("destroy_acknowledgements"), "Invalid acknowledgement",
					fmt.Sprintf("%q: expected ENV:DESCRIPTOR=ACTION", entry))
				continue
			}
		}
		ack, err := client.ParseAck(rest)
		if err != nil {
			diags.AddAttributeError(path.Root("destroy_acknowledgements"), "Invalid acknowledgement", err.Error())
			continue
		}
		out[env] = append(out[env], ack)
	}
	return out
}

// nameAttribute is a display name that defaults to the object's ID. Once known,
// the name is kept from state, so an imported object keeps its name.
func nameAttribute() schema.StringAttribute {
	return schema.StringAttribute{
		Optional:            true,
		Computed:            true,
		MarkdownDescription: "The display name, 1 to 100 characters without surrounding spaces. Defaults to the ID when the object is created.",
		PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
	}
}

// validateName checks a configured name as the server does, so a name the
// server would trim or refuse fails at plan.
func validateName(name types.String, diags *diag.Diagnostics) {
	if name.IsNull() || name.IsUnknown() {
		return
	}
	v := name.ValueString()
	if v != strings.TrimSpace(v) || v == "" || utf8.RuneCountInString(v) > 100 {
		diags.AddAttributeError(path.Root("name"), "Invalid name",
			fmt.Sprintf("%q: names are 1 to 100 characters without surrounding spaces", v))
	}
}

// nameOrID is the name to create an object with: the configured one, or the ID.
func nameOrID(name, id types.String) string {
	if name.IsNull() || name.IsUnknown() {
		return id.ValueString()
	}
	return name.ValueString()
}

// importSplit splits an import ID of the form `A/B`.
func importSplit(id, form string, diags *diag.Diagnostics) (string, string, bool) {
	a, b, ok := strings.Cut(id, "/")
	if !ok || a == "" || b == "" || strings.Contains(b, "/") {
		diags.AddError("Invalid import ID", fmt.Sprintf("%q: expected %s", id, form))
		return "", "", false
	}
	return a, b, true
}
