package provider

import (
	"context"
	"fmt"
	"maps"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/dynamicplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// resourceDefinition is the `openporch_resource_definition` resource.
type resourceDefinition struct {
	client *client.Client
}

var (
	_ resource.ResourceWithImportState    = &resourceDefinition{}
	_ resource.ResourceWithModifyPlan     = &resourceDefinition{}
	_ resource.ResourceWithValidateConfig = &resourceDefinition{}
)

func newResourceDefinition() resource.Resource { return &resourceDefinition{} }

type definitionModel struct {
	ID               types.String  `tfsdk:"id"`
	Name             types.String  `tfsdk:"name"`
	Type             types.String  `tfsdk:"type"`
	DriverType       types.String  `tfsdk:"driver_type"`
	DriverAccount    types.String  `tfsdk:"driver_account"`
	DriverInputs     types.Dynamic `tfsdk:"driver_inputs"`
	DriverSecrets    types.Dynamic `tfsdk:"driver_secrets"`
	DriverSecretRefs types.Dynamic `tfsdk:"driver_secret_refs"`
	Written          types.Dynamic `tfsdk:"written_secret_refs"`
	Provision        types.Dynamic `tfsdk:"provision"`
}

func (r *resourceDefinition) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_resource_definition"
}

func (r *resourceDefinition) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A resource definition: which driver provisions a resource type, with which " +
			"inputs. It matches nothing until it has criteria, which are " +
			"`openporch_resource_definition_criteria` resources. Placeholders such as " +
			"`${context.app.id}` are written `$${context.app.id}` in HCL, so OpenTofu does not " +
			"interpolate them. Secret values in `driver_secrets` are written to the built-in secret " +
			"store and kept in the OpenTofu state, so protect the state; see the provider documentation. " +
			"Import it by ID; secrets are imported as references.",
		Attributes: map[string]schema.Attribute{
			"id": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The definition ID. Changing it replaces the definition."},
			"name": nameAttribute(),
			"type": schema.StringAttribute{Required: true, PlanModifiers: replace,
				MarkdownDescription: "The resource type it provisions, such as `postgres`. Changing it replaces the definition."},
			"driver_type": schema.StringAttribute{Required: true,
				MarkdownDescription: "The driver, such as `openporch/opentofu`."},
			"driver_account": schema.StringAttribute{Optional: true, Validators: []validator.String{notEmpty{}},
				MarkdownDescription: "The cloud account the driver uses, or a `$${resources....account}` placeholder."},
			"driver_inputs": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "The driver's plain inputs, an object such as `{ script = \"…\", variables = { … } }`."},
			"driver_secrets": schema.DynamicAttribute{Optional: true, Sensitive: true,
				MarkdownDescription: "Secret inputs, an object of strings nested like the driver expects, such as " +
					"`{ variables = { password = var.db_password } }`. A literal value is written to the " +
					"built-in store; a whole placeholder such as `$${resources.db.outputs.password}` stays a " +
					"template. The server never returns written values; `written_secret_refs` records the " +
					"versions written, and a version changed outside OpenTofu plans a write of the value again."},
			"driver_secret_refs": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "Secret inputs as references to secrets already in a store, nested like " +
					"`driver_secrets`, with `{ store, ref, version }` leaves. A path cannot be in both. " +
					"Referencing a secret the definition did not write needs a secret-use grant."},
			"written_secret_refs": schema.DynamicAttribute{Computed: true,
				PlanModifiers:       []planmodifier.Dynamic{dynamicplanmodifier.UseStateForUnknown()},
				MarkdownDescription: "Where each literal value of `driver_secrets` was written, nested like it."},
			"provision": schema.DynamicAttribute{Optional: true,
				MarkdownDescription: "Co-provisioned resources, an object keyed by `TYPE[.CLASS][#ID]` with " +
					"`is_dependent`, `match_dependents` and `params`."},
		},
	}
}

func (r *resourceDefinition) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.client = configureClient(req, resp)
}

func (r *resourceDefinition) ValidateConfig(ctx context.Context, req resource.ValidateConfigRequest, resp *resource.ValidateConfigResponse) {
	var config definitionModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	validateName(config.Name, &resp.Diagnostics)
	// Only the shapes are checked here; values the server checks on write. An
	// unknown value is checked at apply.
	if x, err := dynamicToJSON(ctx, config.DriverSecrets); err == nil {
		if _, err := stringLeaves(x); err != nil {
			resp.Diagnostics.AddAttributeError(path.Root("driver_secrets"), "Invalid secret inputs", err.Error())
		}
	}
	if x, err := dynamicToJSON(ctx, config.DriverSecretRefs); err == nil {
		if _, err := refLeaves(x); err != nil {
			resp.Diagnostics.AddAttributeError(path.Root("driver_secret_refs"), "Invalid secret references", err.Error())
		}
	}
	for attr, v := range map[string]types.Dynamic{"driver_inputs": config.DriverInputs, "provision": config.Provision} {
		if x, err := dynamicToJSON(ctx, v); err == nil && x != nil {
			if _, ok := x.(map[string]any); !ok {
				resp.Diagnostics.AddAttributeError(path.Root(attr), "Invalid value", "Expected an object.")
			}
		}
	}
}

// ModifyPlan marks the written references unknown when secret inputs change.
func (r *resourceDefinition) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || req.State.Raw.IsNull() {
		return
	}
	var plan, state definitionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if !plan.Type.IsUnknown() && !plan.Type.Equal(state.Type) && r.client != nil {
		// Replacing the definition deletes its criteria with it, which the
		// criteria resources only notice on the next refresh.
		if d, err := r.client.Definition(ctx, state.ID.ValueString()); err == nil && len(d.Criteria) > 0 {
			resp.Diagnostics.AddAttributeWarning(path.Root("type"), "Criteria are deleted with the definition",
				"Changing `type` replaces the definition, and the server deletes its criteria with it. "+
					"Give each `openporch_resource_definition_criteria` of it "+
					"`lifecycle { replace_triggered_by = [openporch_resource_definition.NAME] }` so they are "+
					"created again in the same apply; otherwise the definition matches nothing until the "+
					"next apply recreates them.")
		}
	}
	written := path.Root("written_secret_refs")
	if plan.DriverSecrets.IsNull() {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.DynamicNull())...)
	} else if !plan.DriverSecrets.Equal(state.DriverSecrets) {
		resp.Diagnostics.Append(resp.Plan.SetAttribute(ctx, written, types.DynamicUnknown())...)
	}
}

// object reads a dynamic attribute holding a JSON object, or nil when it is null.
func object(ctx context.Context, name string, v types.Dynamic) (map[string]any, error) {
	x, err := dynamicToJSON(ctx, v)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", name, err)
	}
	if x == nil {
		return nil, nil
	}
	m, ok := x.(map[string]any)
	if !ok {
		return nil, fmt.Errorf("%s: expected an object", name)
	}
	return m, nil
}

// body is the write that makes the server's definition match m. A secret the
// provider wrote before and that is unchanged is sent as its reference, so it
// keeps its version.
func (m definitionModel) body(ctx context.Context, prior *definitionModel, name string) (client.DefinitionWrite, error) {
	w := client.DefinitionWrite{
		ID: m.ID.ValueString(), Name: name, Type: m.Type.ValueString(),
		DriverType: m.DriverType.ValueString(), DriverAccount: m.DriverAccount.ValueString(),
	}
	values, err := object(ctx, "driver_inputs", m.DriverInputs)
	if err != nil {
		return w, err
	}
	if values == nil {
		values = map[string]any{}
	}
	if w.Provision, err = object(ctx, "provision", m.Provision); err != nil {
		return w, err
	}
	secretsJSON, err := object(ctx, "driver_secrets", m.DriverSecrets)
	if err != nil {
		return w, err
	}
	secrets, err := stringLeaves(nilIfEmpty(secretsJSON))
	if err != nil {
		return w, fmt.Errorf("driver_secrets: %w", err)
	}
	refsJSON, err := object(ctx, "driver_secret_refs", m.DriverSecretRefs)
	if err != nil {
		return w, err
	}
	configRefs, err := refLeaves(nilIfEmpty(refsJSON))
	if err != nil {
		return w, fmt.Errorf("driver_secret_refs: %w", err)
	}
	priorSecrets, priorWritten := map[inputPath]string{}, map[inputPath]client.SecretRef{}
	if prior != nil {
		priorSecrets, priorWritten = prior.secretLeaves(ctx), prior.writtenLeaves(ctx)
	}
	inline := map[inputPath]string{}
	refs := maps.Clone(configRefs)
	for p, v := range secrets {
		if old, ok := priorSecrets[p]; ok && old == v {
			if ref, ok := priorWritten[p]; ok {
				refs[p] = ref
				continue
			}
		}
		inline[p] = v
	}
	w.DriverInputs = client.DriverInputs{
		Values:     values,
		Secrets:    nest(inline, func(s string) any { return s }),
		SecretRefs: nest(refs, refJSON),
	}
	return w, nil
}

func nilIfEmpty(m map[string]any) any {
	if m == nil {
		return nil
	}
	return m
}

// secretLeaves are the configured secret inputs; values that cannot be read
// count as none.
func (m definitionModel) secretLeaves(ctx context.Context) map[inputPath]string {
	x, err := object(ctx, "driver_secrets", m.DriverSecrets)
	if err != nil {
		return map[inputPath]string{}
	}
	leaves, err := stringLeaves(nilIfEmpty(x))
	if err != nil {
		return map[inputPath]string{}
	}
	return leaves
}

// writtenLeaves are the references of secrets the provider wrote.
func (m definitionModel) writtenLeaves(ctx context.Context) map[inputPath]client.SecretRef {
	x, err := object(ctx, "written_secret_refs", m.Written)
	if err != nil {
		return map[inputPath]client.SecretRef{}
	}
	leaves, err := refLeaves(nilIfEmpty(x))
	if err != nil {
		return map[inputPath]client.SecretRef{}
	}
	return leaves
}

// keepOrSet is current when it holds the same JSON as server, so values keep the
// types they were configured with; null stays null for an empty object.
func keepOrSet(ctx context.Context, current types.Dynamic, server map[string]any, normalize func(any) any) (types.Dynamic, error) {
	cur, err := dynamicToJSON(ctx, current)
	if err == nil {
		if cur == nil && len(server) == 0 {
			return current, nil
		}
		if sameJSON(normalize(cur), normalize(map[string]any(server))) {
			return current, nil
		}
	}
	if server == nil {
		server = map[string]any{}
	}
	return jsonToDynamic(ctx, server)
}

func identity(x any) any { return x }

// normalizeRefs rebuilds references as the server returns them: without empty
// objects or versions.
func normalizeRefs(x any) any {
	leaves, err := refLeaves(x)
	if err != nil {
		return x
	}
	return nest(leaves, refJSON)
}

// normalizeProvision fills in the defaults the server returns for co-provisioning
// entries, so a configuration that leaves them out compares equal.
func normalizeProvision(x any) any {
	m, ok := x.(map[string]any)
	if !ok {
		return x
	}
	out := make(map[string]any, len(m))
	for k, v := range m {
		entry, ok := v.(map[string]any)
		if !ok {
			out[k] = v
			continue
		}
		e := maps.Clone(entry)
		for _, flag := range []string{"is_dependent", "match_dependents"} {
			if _, set := e[flag]; !set {
				e[flag] = false
			}
		}
		if p, set := e["params"]; set && p == nil {
			delete(e, "params")
		}
		out[k] = e
	}
	return out
}

// apply records the server's definition in m. fresh says the provider has just
// written it, so the references the server returns for literal secrets are the
// ones it wrote; otherwise only references matching `written_secret_refs` are.
func (m *definitionModel) apply(ctx context.Context, d *client.Definition, fresh bool) diag.Diagnostics {
	var diags diag.Diagnostics
	fail := func(err error) diag.Diagnostics {
		diags.AddError("Unexpected resource definition", err.Error())
		return diags
	}
	m.ID, m.Name, m.Type = types.StringValue(d.ID), types.StringValue(d.Name), types.StringValue(d.Type)
	m.DriverType = types.StringValue(d.DriverType)
	m.DriverAccount = types.StringNull()
	if d.DriverAccount != "" {
		m.DriverAccount = types.StringValue(d.DriverAccount)
	}
	var err error
	if m.DriverInputs, err = keepOrSet(ctx, m.DriverInputs, d.DriverInputs.Values, identity); err != nil {
		return fail(err)
	}
	if m.Provision, err = keepOrSet(ctx, m.Provision, d.Provision, normalizeProvision); err != nil {
		return fail(err)
	}

	templates, err := stringLeaves(nilIfEmpty(d.DriverInputs.Secrets))
	if err != nil {
		return fail(err)
	}
	serverRefs, err := refLeaves(nilIfEmpty(d.DriverInputs.SecretRefs))
	if err != nil {
		return fail(err)
	}
	configured := m.secretLeaves(ctx)
	priorWritten := m.writtenLeaves(ctx)
	// Literal secrets stay in `driver_secrets` while the server has the version
	// written; one changed outside OpenTofu is forgotten, so the plan writes it
	// again. Templates stay while the server has the same one.
	kept := map[inputPath]string{}
	written := map[inputPath]client.SecretRef{}
	for p, v := range configured {
		if t, ok := templates[p]; ok {
			if t == v {
				kept[p] = v
			}
			continue
		}
		ref, ok := serverRefs[p]
		if !ok {
			continue
		}
		if old, ok := priorWritten[p]; !fresh && (!ok || old != ref) {
			continue
		}
		kept[p] = v
		written[p] = ref
	}
	for p, t := range templates {
		if _, ok := configured[p]; !ok {
			kept[p] = t
		}
	}
	if !maps.Equal(kept, configured) {
		if m.DriverSecrets, err = jsonToDynamic(ctx, nest(kept, func(s string) any { return s })); err != nil {
			return fail(err)
		}
	}
	if m.DriverSecrets.IsNull() {
		m.Written = types.DynamicNull()
	} else if m.Written, err = jsonToDynamic(ctx, nest(written, refJSON)); err != nil {
		return fail(err)
	}

	refs := map[inputPath]client.SecretRef{}
	for p, ref := range serverRefs {
		if _, ok := configured[p]; !ok {
			refs[p] = ref
		}
	}
	if m.DriverSecretRefs, err = keepOrSet(ctx, m.DriverSecretRefs, nest(refs, refJSON), normalizeRefs); err != nil {
		return fail(err)
	}
	return diags
}

func (r *resourceDefinition) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan definitionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body, err := plan.body(ctx, nil, nameOrID(plan.Name, plan.ID))
	if err != nil {
		resp.Diagnostics.AddError("Invalid resource definition", err.Error())
		return
	}
	d, err := r.client.CreateDefinition(ctx, body)
	if err != nil {
		apiError(&resp.Diagnostics, "creating the resource definition", err)
		return
	}
	resp.Diagnostics.Append(plan.apply(ctx, d, true)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *resourceDefinition) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state definitionModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	d, err := r.client.Definition(ctx, state.ID.ValueString())
	if client.IsNotFound(err) {
		resp.State.RemoveResource(ctx)
		return
	}
	if err != nil {
		apiError(&resp.Diagnostics, "reading the resource definition", err)
		return
	}
	resp.Diagnostics.Append(state.apply(ctx, d, false)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, state)...)
}

func (r *resourceDefinition) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state definitionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	name := plan.Name.ValueString()
	if plan.Name.IsUnknown() || plan.Name.IsNull() {
		name = state.Name.ValueString()
	}
	body, err := plan.body(ctx, &state, name)
	if err != nil {
		resp.Diagnostics.AddError("Invalid resource definition", err.Error())
		return
	}
	d, err := r.client.ReplaceDefinition(ctx, plan.ID.ValueString(), body)
	if err != nil {
		apiError(&resp.Diagnostics, "updating the resource definition", err)
		return
	}
	// What was just written is not drift.
	resp.Diagnostics.Append(plan.apply(ctx, d, true)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, plan)...)
}

func (r *resourceDefinition) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state definitionModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	err := r.client.DeleteDefinition(ctx, state.ID.ValueString())
	if err != nil && !client.IsNotFound(err) {
		apiError(&resp.Diagnostics, "deleting the resource definition", err)
	}
}

func (r *resourceDefinition) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resource.ImportStatePassthroughID(ctx, path.Root("id"), req, resp)
}
