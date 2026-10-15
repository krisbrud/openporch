// Package provider implements the openporch OpenTofu provider.
package provider

import (
	"context"
	"errors"
	"os"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// Address is the provider's registry address.
const Address = "registry.opentofu.org/openporch/openporch"

// The environment variables that supply settings the configuration leaves out.
const (
	EnvURL   = "OPENPORCH_URL"
	EnvOrg   = "OPENPORCH_ORG"
	EnvToken = "OPENPORCH_TOKEN"
)

type openporchProvider struct {
	version string
	getenv  func(string) string
}

// New returns a function that creates the provider, for providerserver.
func New(version string) func() provider.Provider {
	return newWithEnv(version, os.Getenv)
}

func newWithEnv(version string, getenv func(string) string) func() provider.Provider {
	return func() provider.Provider {
		return &openporchProvider{version: version, getenv: getenv}
	}
}

type providerModel struct {
	URL   types.String `tfsdk:"url"`
	OrgID types.String `tfsdk:"org_id"`
	Token types.String `tfsdk:"token"`
}

func (p *openporchProvider) Metadata(_ context.Context, _ provider.MetadataRequest, resp *provider.MetadataResponse) {
	resp.TypeName = "openporch"
	resp.Version = p.version
}

func (p *openporchProvider) Schema(_ context.Context, _ provider.SchemaRequest, resp *provider.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "Manages the platform-side objects of an [openporch](https://github.com/krisbrud/openporch) " +
			"organization: applications, environments, resource definitions and their criteria, cloud accounts, " +
			"values and access. Deployments are not resources; use `porch score deploy` or " +
			"`porch workload deploy` for those.",
		Attributes: map[string]schema.Attribute{
			"url": schema.StringAttribute{
				MarkdownDescription: "The server URL, such as `https://porch.example.com`. Plain `http` is only " +
					"allowed for loopback addresses. Defaults to `" + EnvURL + "`.",
				Optional: true,
			},
			"org_id": schema.StringAttribute{
				MarkdownDescription: "The organization ID. Defaults to `" + EnvOrg + "`.",
				Optional:            true,
			},
			"token": schema.StringAttribute{
				MarkdownDescription: "An API token for a service user. Defaults to `" + EnvToken + "`.",
				Optional:            true,
				Sensitive:           true,
			},
		},
	}
}

// setting is a configured value, or the environment variable when the
// configuration leaves it out or sets it to an empty string. Unknown values are
// reported as unknown.
func (p *openporchProvider) setting(v types.String, env string) (string, bool) {
	if v.IsUnknown() {
		return "", false
	}
	if value := strings.TrimSpace(v.ValueString()); value != "" {
		return value, true
	}
	return strings.TrimSpace(p.getenv(env)), true
}

func (p *openporchProvider) Configure(ctx context.Context, req provider.ConfigureRequest, resp *provider.ConfigureResponse) {
	var config providerModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &config)...)
	if resp.Diagnostics.HasError() {
		return
	}
	settings := []struct {
		attr  string
		value types.String
		env   string
	}{
		{"url", config.URL, EnvURL},
		{"org_id", config.OrgID, EnvOrg},
		{"token", config.Token, EnvToken},
	}
	values := map[string]string{}
	for _, s := range settings {
		value, known := p.setting(s.value, s.env)
		if !known {
			resp.Diagnostics.AddAttributeError(path.Root(s.attr), "Unknown openporch "+s.attr,
				"The provider's `"+s.attr+"` is not known until apply. Set it to a value known at plan time, "+
					"or leave it out and set `"+s.env+"`.")
			continue
		}
		if value == "" {
			resp.Diagnostics.AddAttributeError(path.Root(s.attr), "Missing openporch "+s.attr,
				"Set the provider's `"+s.attr+"` argument or the `"+s.env+"` environment variable.")
			continue
		}
		values[s.attr] = value
	}
	if resp.Diagnostics.HasError() {
		return
	}
	c, err := client.New(values["url"], values["org_id"], values["token"], "openporch-provider/"+p.version)
	if errors.Is(err, client.ErrInvalidToken) {
		resp.Diagnostics.AddAttributeError(path.Root("token"), "Invalid openporch token", err.Error())
		return
	}
	if err != nil {
		resp.Diagnostics.AddAttributeError(path.Root("url"), "Invalid openporch url", err.Error())
		return
	}
	// Checking the token here makes a wrong setting fail at plan, before any
	// resource is read or changed.
	if _, err := c.CurrentUser(ctx); err != nil {
		resp.Diagnostics.AddError("Cannot authenticate with openporch",
			"Checking the provider's token with organization `"+values["org_id"]+"` failed: "+err.Error())
		return
	}
	resp.ResourceData = c
	resp.DataSourceData = c
}

func (p *openporchProvider) Resources(context.Context) []func() resource.Resource {
	return []func() resource.Resource{
		newApplication, newEnvironmentType, newEnvironment, newResourceClass, newValue, newCloudAccount,
	}
}

func (p *openporchProvider) DataSources(context.Context) []func() datasource.DataSource {
	return []func() datasource.DataSource{newCurrentUser}
}
