package provider

import (
	"context"
	"net/url"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/objectdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/objectplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// driver is the `openporch_driver` resource: an external HTTP driver's
// registration.
type driver struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &driver{}
	_ resource.ResourceWithModifyPlan     = &driver{}
	_ resource.ResourceWithValidateConfig = &driver{}
)

func newDriver() resource.Resource { return &driver{} }

type driverModel struct {
	ID              types.String  `tfsdk:"id"`
	Target          types.String  `tfsdk:"target"`
	Token           types.String  `tfsdk:"token"`
	TokenRef        types.Object  `tfsdk:"token_ref"`
	Written         types.Object  `tfsdk:"written_token_ref"`
	ProtocolVersion types.String  `tfsdk:"protocol_version"`
	CookieVersion   types.Int64   `tfsdk:"cookie_version"`
	InputsSchema    types.Dynamic `tfsdk:"inputs_schema"`
	OutputsSchema   types.Dynamic `tfsdk:"outputs_schema"`
	Capabilities    types.Object  `tfsdk:"capabilities"`
}

var driverTargetsAttrTypes = map[string]attr.Type{
	"resources": types.BoolType,
	"workloads": types.BoolType,
	"triggers":  types.BoolType,
}

var driverCapabilitiesAttrTypes = map[string]attr.Type{
	"targets": types.ObjectType{AttrTypes: driverTargetsAttrTypes},
	"observe": types.BoolType,
	"recover": types.BoolType,
	"cancel":  types.BoolType,
}

// capabilitiesValue is the capabilities object for c; the server's defaults when
// c is nil.
func capabilitiesValue(c *client.DriverCapabilities) types.Object {
	if c == nil {
		c = &client.DriverCapabilities{Targets: client.DriverTargets{Resources: true}}
	}
	return types.ObjectValueMust(driverCapabilitiesAttrTypes, map[string]attr.Value{
		"targets": types.ObjectValueMust(driverTargetsAttrTypes, map[string]attr.Value{
			"resources": types.BoolValue(c.Targets.Resources),
			"workloads": types.BoolValue(c.Targets.Workloads),
			"triggers":  types.BoolValue(c.Targets.Triggers),
		}),
		"observe": types.BoolValue(c.Observe),
		"recover": types.BoolValue(c.Recover),
		"cancel":  types.BoolValue(c.Cancel),
	})
}

// knownBools adds the known booleans of o to out.
func knownBools(o types.Object, out map[string]any) {
	for k, v := range o.Attributes() {
		if b, ok := v.(types.Bool); ok && !b.IsNull() && !b.IsUnknown() {
			out[k] = b.ValueBool()
		}
	}
}

// capabilitiesBody is the capabilities to send: the fields that are known, so
// the server's defaults fill in the rest.
func capabilitiesBody(o types.Object) map[string]any {
	if o.IsNull() || o.IsUnknown() {
		return nil
	}
	out := map[string]any{}
	knownBools(o, out)
	if t, ok := o.Attributes()["targets"].(types.Object); ok && !t.IsNull() && !t.IsUnknown() {
		targets := map[string]any{}
		knownBools(t, targets)
		out["targets"] = targets
	}
	return out
}

// normalizeTarget approximates how the server's URL parser writes an `https`
// target: a lowercase scheme and host, no default port and at least `/` as the
// path. It returns s unchanged when it does not parse.
func normalizeTarget(s string) string {
	u, err := url.Parse(s)
	if err != nil || u.Host == "" {
		return s
	}
	u.Scheme = strings.ToLower(u.Scheme)
	host := strings.ToLower(u.Hostname())
	if strings.Contains(host, ":") {
		host = "[" + host + "]"
	}
	if port := u.Port(); port != "" && (u.Scheme != "https" || port != "443") {
		host += ":" + port
	}
	u.Host = host
	if u.Path == "" && u.RawPath == "" {
		u.Path = "/"
	}
	return u.String()
}

func boolAttr(description string, value bool) schema.BoolAttribute {
	return schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(value),
		MarkdownDescription: description}
}

func (r *driver) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_driver"
}

func (r *driver) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	targets := map[string]schema.Attribute{
		"resources": boolAttr("The driver provisions resources other than workloads. Defaults to `true`.", true),
		"workloads": boolAttr("The driver provisions workloads. Defaults to `false`.", false),
		"triggers":  boolAttr("The driver provisions workload triggers; recorded only. Defaults to `false`.", false),
	}
	defaultTargets := types.ObjectValueMust(driverTargetsAttrTypes, map[string]attr.Value{
		"resources": types.BoolValue(true), "workloads": types.BoolValue(false), "triggers": types.BoolValue(false),
	})
	resp.Schema = schema.Schema{
		MarkdownDescription: "An external HTTP driver's registration: where openporch reaches the driver, the " +
			"bearer token it sends and what the driver supports. Resource definitions name it in `driver_type`. " +
			"Changes update the registration in place, so rotating the token or moving the target keeps the " +
			"definitions that use it; the server refuses a change while an operation the driver ran has an " +
			"outcome that is not known yet, or one that stops the driver targeting the type of a definition " +
			"using it. A `token` is written to the built-in secret store and kept in the OpenTofu state, so " +
			"protect the state; see the provider documentation. A driver that definitions or active resources " +
			"use cannot be deleted. Import it by ID; the token is imported as a reference. Needs a server with " +
			"`PUT /orgs/{org}/resources/drivers/{driver}`.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.RequiresReplace()},
				MarkdownDescription: "The driver ID, `<namespace>/<name>`; the `openporch` namespace is reserved. " +
					"Changing it replaces the driver."},
			"target": schema.StringAttribute{Required: true,
				MarkdownDescription: "The `https` URL the driver is reached at, without credentials, query or " +
					"fragment. The server checks only that it is a valid `https` URL; deployments find out " +
					"whether it can be reached."},
			"token": schema.StringAttribute{Optional: true, Sensitive: true,
				MarkdownDescription: "The bearer token sent to the driver, written to the built-in store. Changing " +
					"it writes a new version. The server never returns it; `written_token_ref` records the version " +
					"written, and a version changed outside OpenTofu plans a write of this token again. Conflicts " +
					"with `token_ref`; with neither, the driver gets no token."},
			"token_ref": schema.SingleNestedAttribute{Optional: true, Attributes: secretRefAttributes(false),
				MarkdownDescription: "The bearer token as a reference to a secret already in a store. Referencing " +
					"a secret openporch did not write for this driver needs a secret-use grant, and of the " +
					"driver's own secrets only the token it holds now may be referenced."},
			"written_token_ref": schema.SingleNestedAttribute{Computed: true, Attributes: secretRefAttributes(true),
				MarkdownDescription: "Where `token` was written.",
				PlanModifiers:       []planmodifier.Object{objectplanmodifier.UseStateForUnknown()}},
			"protocol_version": schema.StringAttribute{Optional: true, Computed: true,
				PlanModifiers: []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "The protocol the driver speaks. Left out, the server's default " +
					"(`openporch-http/1`) is pinned when the driver is registered, and kept afterwards."},
			"cookie_version": schema.Int64Attribute{Optional: true, Computed: true,
				PlanModifiers: []planmodifier.Int64{int64planmodifier.UseStateForUnknown()},
				MarkdownDescription: "The cookie format version the driver writes. Left out, the server's default " +
					"(`1`) is pinned when the driver is registered, and kept afterwards."},
			"inputs_schema": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "JSON Schema of the driver inputs, an object, recorded for clients."},
			"outputs_schema": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "JSON Schema of the driver's outputs, an object, recorded for clients."},
			"capabilities": schema.SingleNestedAttribute{Optional: true, Computed: true,
				Default: objectdefault.StaticValue(types.ObjectValueMust(driverCapabilitiesAttrTypes, map[string]attr.Value{
					"targets": defaultTargets, "observe": types.BoolValue(false),
					"recover": types.BoolValue(false), "cancel": types.BoolValue(false),
				})),
				MarkdownDescription: "What the driver supports beyond provisioning and removing resources. " +
					"Left out, the driver provisions resources other than workloads and nothing more. The " +
					"protocol has no preview call, so a driver never previews.",
				Attributes: map[string]schema.Attribute{
					"targets": schema.SingleNestedAttribute{Optional: true, Computed: true, Attributes: targets,
						Default:             objectdefault.StaticValue(defaultTargets),
						MarkdownDescription: "What the driver provisions."},
					"observe": boolAttr("`GET` reports the resource's real state. Defaults to `false`.", false),
					"recover": boolAttr("A retried operation converges, so a `5xx` to a mutation may be retried. "+
						"Defaults to `false`.", false),
					"cancel": boolAttr("A timed-out operation can be cancelled. Defaults to `false`.", false),
				},
			},
		},
	}
}

func (r *driver) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *driver) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config driverModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if !config.Token.IsNull() && !config.TokenRef.IsNull() {
		resp.Diagnostics.AddAttributeError(path.Root("token_ref"), "Conflicting token",
			"Set at most one of `token` and `token_ref`.")
	}
}

// ModifyPlan marks the written reference unknown when a new token is written.
func (r *driver) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || req.State.Raw.IsNull() {
		return
	}
	var plan, state driverModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	written := path.Root("written_token_ref")
	if plan.Token.IsNull() {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.ObjectNull(secretRefAttrTypes))...)
	} else if !plan.Token.Equal(state.Token) {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.ObjectUnknown(secretRefAttrTypes))...)
	}
}

// body is the write that makes the server's registration match m. A token the
// provider wrote before and that is unchanged is sent as its reference, so it
// keeps its version.
func (m driverModel) body(ctx context.Context, prior *driverModel) (client.DriverWrite, diag.Diagnostics) {
	var diags diag.Diagnostics
	w := client.DriverWrite{
		ID:           m.ID.ValueString(),
		Target:       m.Target.ValueString(),
		Capabilities: capabilitiesBody(m.Capabilities),
	}
	if !m.ProtocolVersion.IsNull() && !m.ProtocolVersion.IsUnknown() {
		w.ProtocolVersion = m.ProtocolVersion.ValueString()
	}
	if !m.CookieVersion.IsNull() && !m.CookieVersion.IsUnknown() {
		v := m.CookieVersion.ValueInt64()
		w.CookieVersion = &v
	}
	for _, s := range []struct {
		name string
		from types.Dynamic
		to   *any
	}{{"inputs_schema", m.InputsSchema, &w.InputsSchema}, {"outputs_schema", m.OutputsSchema, &w.OutputsSchema}} {
		v, err := dynamicToJSON(ctx, s.from)
		if err != nil {
			diags.AddAttributeError(path.Root(s.name), "Invalid schema", err.Error())
			continue
		}
		if v != nil {
			*s.to = v
		}
	}
	switch {
	case !m.TokenRef.IsNull():
		w.Token = &client.SecretWrite{SecretRef: refFromObject(m.TokenRef)}
	case !m.Token.IsNull():
		if prior != nil && prior.Token.Equal(m.Token) {
			if ref := refFromObject(prior.Written); ref != nil {
				w.Token = &client.SecretWrite{SecretRef: ref}
				break
			}
		}
		v := m.Token.ValueString()
		w.Token = &client.SecretWrite{Value: &v}
	}
	return w, diags
}

// apply records the server's registration in m.
func (m *driverModel) apply(ctx context.Context, d *client.Driver) diag.Diagnostics {
	var diags diag.Diagnostics
	m.ID = types.StringValue(d.ID)
	// The server writes the target as its URL parser does; a configured target
	// it only normalized is kept as configured.
	if m.Target.IsNull() || m.Target.IsUnknown() || normalizeTarget(m.Target.ValueString()) != d.Target {
		m.Target = types.StringValue(d.Target)
	}
	m.ProtocolVersion = types.StringValue(d.ProtocolVersion)
	m.CookieVersion = types.Int64Value(d.CookieVersion)
	m.Capabilities = capabilitiesValue(d.Capabilities)

	for _, s := range []struct {
		name   string
		field  *types.Dynamic
		server map[string]any
	}{{"inputs_schema", &m.InputsSchema, d.InputsSchema}, {"outputs_schema", &m.OutputsSchema, d.OutputsSchema}} {
		current, err := dynamicToJSON(ctx, *s.field)
		switch {
		case err == nil && current == nil && s.server == nil:
		case err == nil && s.server != nil && sameJSON(current, s.server):
		case s.server == nil:
			*s.field = types.DynamicNull()
		default:
			v, err := jsonToDynamic(ctx, s.server)
			if err != nil {
				diags.AddError("Unexpected driver "+s.name, err.Error())
				return diags
			}
			*s.field = v
		}
	}

	switch {
	case d.Token == nil:
		// A token removed outside OpenTofu is forgotten, so the plan writes it again.
		m.Token, m.TokenRef = types.StringNull(), types.ObjectNull(secretRefAttrTypes)
		m.Written = types.ObjectNull(secretRefAttrTypes)
	case !m.Token.IsNull():
		written := refFromObject(m.Written)
		if written != nil && *written != *d.Token {
			// Changed outside OpenTofu: forget the token, so the plan writes it
			// again, and the reference, which the provider did not write.
			m.Token = types.StringNull()
			m.Written = types.ObjectNull(secretRefAttrTypes)
			return diags
		}
		m.TokenRef = types.ObjectNull(secretRefAttrTypes)
		m.Written = writtenRef(*d.Token)
	default:
		m.TokenRef = readRef(*d.Token)
		m.Written = types.ObjectNull(secretRefAttrTypes)
	}
	return diags
}

func (r *driver) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan driverModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body, diags := plan.body(ctx, nil)
	resp.Diagnostics.Append(diags...)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.client.CreateDriver(ctx, body)
	if err != nil {
		apiError(&resp.Diagnostics, "registering the driver", err)
		return
	}
	plan.Written = types.ObjectNull(secretRefAttrTypes)
	resp.Diagnostics.Append(plan.apply(ctx, d)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *driver) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state driverModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.client.Driver(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the driver", err)
		return
	}
	if d.Builtin {
		resp.Diagnostics.AddError("Built-in driver",
			"`"+d.ID+"` is a built-in driver; only external drivers can be managed.")
		return
	}
	resp.Diagnostics.Append(state.apply(ctx, d)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *driver) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state driverModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body, diags := plan.body(ctx, &state)
	resp.Diagnostics.Append(diags...)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.client.ReplaceDriver(ctx, plan.ID.ValueString(), body)
	if err != nil {
		apiError(&resp.Diagnostics, "updating the driver", err)
		return
	}
	plan.Written = state.Written
	if !plan.Token.Equal(state.Token) {
		plan.Written = types.ObjectNull(secretRefAttrTypes)
	}
	resp.Diagnostics.Append(plan.apply(ctx, d)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *driver) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state driverModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteDriver(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the driver", err)
	}
}

// ImportState takes the driver ID. The token is imported as its reference,
// since its value cannot be read.
func (r *driver) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
