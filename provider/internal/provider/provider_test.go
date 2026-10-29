package provider

import (
	"context"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

const goodToken = "opt_good-token"

// server answers `current-user` for goodToken in organization `acme` and records
// the organizations asked for.
func server(t *testing.T) *httptest.Server {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Header.Get("Authorization") != "Bearer "+goodToken:
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":"AUTH-001","message":"no valid token or session"}`))
		case r.URL.Path != "/orgs/acme/current-user":
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte(`{"error":"API-001","message":"not found"}`))
		default:
			_, _ = w.Write([]byte(`{"id":"ci","type":"service","name":"CI","role":"administrator"}`))
		}
	}))
	t.Cleanup(srv.Close)
	return srv
}

var configType = tftypes.Object{AttributeTypes: map[string]tftypes.Type{
	"url":    tftypes.String,
	"org_id": tftypes.String,
	"token":  tftypes.String,
}}

// configure runs ConfigureProvider over protocol 6 with the given arguments
// (nil leaves one out, tftypes.UnknownValue makes it unknown) and environment.
func configure(t *testing.T, args map[string]any, env map[string]string) []*tfprotov6.Diagnostic {
	t.Helper()
	values := map[string]tftypes.Value{}
	for _, name := range []string{"url", "org_id", "token"} {
		values[name] = tftypes.NewValue(tftypes.String, args[name])
	}
	config, err := tfprotov6.NewDynamicValue(configType, tftypes.NewValue(configType, values))
	if err != nil {
		t.Fatal(err)
	}
	getenv := func(k string) string { return env[k] }
	srv, err := providerserver.NewProtocol6WithError(newWithEnv("test", getenv)())()
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if _, err := srv.GetProviderSchema(ctx, &tfprotov6.GetProviderSchemaRequest{}); err != nil {
		t.Fatal(err)
	}
	resp, err := srv.ConfigureProvider(ctx, &tfprotov6.ConfigureProviderRequest{
		TerraformVersion: "1.11.5",
		Config:           &config,
	})
	if err != nil {
		t.Fatal(err)
	}
	return resp.Diagnostics
}

func errorText(diags []*tfprotov6.Diagnostic) string {
	var out []string
	for _, d := range diags {
		if d.Severity == tfprotov6.DiagnosticSeverityError {
			out = append(out, d.Summary+": "+d.Detail)
		}
	}
	return strings.Join(out, "\n")
}

func TestConfigureFromEnvironment(t *testing.T) {
	srv := server(t)
	diags := configure(t, nil, map[string]string{
		EnvURL: srv.URL, EnvOrg: "acme", EnvToken: goodToken,
	})
	if e := errorText(diags); e != "" {
		t.Fatalf("configuring from the environment failed:\n%s", e)
	}
}

func TestArgumentsOverrideTheEnvironment(t *testing.T) {
	srv := server(t)
	diags := configure(t,
		map[string]any{"url": srv.URL, "org_id": "acme", "token": goodToken},
		map[string]string{EnvURL: "https://elsewhere.example.com", EnvOrg: "other", EnvToken: "opt_other"},
	)
	if e := errorText(diags); e != "" {
		t.Fatalf("configuring from arguments failed:\n%s", e)
	}
}

func TestMissingSettingsNameTheirVariables(t *testing.T) {
	diags := configure(t, map[string]any{"url": "http://127.0.0.1:1"}, nil)
	e := errorText(diags)
	for _, want := range []string{EnvOrg, EnvToken} {
		if !strings.Contains(e, want) {
			t.Errorf("the errors do not mention %s:\n%s", want, e)
		}
	}
	if strings.Contains(e, EnvURL) {
		t.Errorf("the configured url was reported missing:\n%s", e)
	}
}

func TestEmptyArgumentsFallBackToTheEnvironment(t *testing.T) {
	srv := server(t)
	diags := configure(t,
		map[string]any{"url": "", "org_id": "", "token": ""},
		map[string]string{EnvURL: srv.URL, EnvOrg: "acme", EnvToken: goodToken},
	)
	if e := errorText(diags); e != "" {
		t.Fatalf("empty arguments did not fall back to the environment:\n%s", e)
	}
}

func TestUnknownSettingsAreRefused(t *testing.T) {
	srv := server(t)
	diags := configure(t, map[string]any{"url": srv.URL, "org_id": "acme", "token": tftypes.UnknownValue}, nil)
	if e := errorText(diags); !strings.Contains(e, "not known until apply") {
		t.Fatalf("expected an unknown-token error, got:\n%s", e)
	}
}

func TestAWrongTokenFailsAtConfigureWithoutShowingIt(t *testing.T) {
	srv := server(t)
	diags := configure(t, map[string]any{"url": srv.URL, "org_id": "acme", "token": "opt_wrong-token"}, nil)
	e := errorText(diags)
	if !strings.Contains(e, "AUTH-001") {
		t.Fatalf("expected an authentication error, got:\n%s", e)
	}
	if strings.Contains(e, "opt_wrong-token") {
		t.Fatalf("the error shows the token:\n%s", e)
	}
}

func TestPlainHTTPIsRefusedOffLoopback(t *testing.T) {
	diags := configure(t, map[string]any{"url": "http://porch.example.com", "org_id": "acme", "token": goodToken}, nil)
	if e := errorText(diags); !strings.Contains(e, "loopback") {
		t.Fatalf("expected a transport error, got:\n%s", e)
	}
}

// TestAnUntrustedCertificateIsRefusedBeforeTheTokenIsSent is the provider half
// of "Invalid server certificate": a server whose certificate no trusted
// authority signed fails configuration, and no request, so no token, reaches it.
func TestAnUntrustedCertificateIsRefusedBeforeTheTokenIsSent(t *testing.T) {
	var reached atomic.Bool
	srv := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		reached.Store(true)
		_, _ = w.Write([]byte(`{"id":"ci","type":"service","name":"CI","role":"administrator"}`))
	}))
	// The refused handshakes are expected.
	srv.Config.ErrorLog = log.New(io.Discard, "", 0)
	srv.StartTLS()
	t.Cleanup(srv.Close)
	if !strings.HasPrefix(srv.URL, "https://") {
		t.Fatalf("not a TLS server: %s", srv.URL)
	}

	diags := configure(t, map[string]any{"url": srv.URL, "org_id": "acme", "token": goodToken}, nil)
	e := errorText(diags)
	if !strings.Contains(e, "certificate") {
		t.Fatalf("expected a certificate error, got:\n%s", e)
	}
	if strings.Contains(e, goodToken) {
		t.Fatalf("the error shows the token:\n%s", e)
	}
	if reached.Load() {
		t.Fatal("a request reached the server with an untrusted certificate")
	}
}

func TestTheTokenIsSensitive(t *testing.T) {
	srv, err := providerserver.NewProtocol6WithError(New("test")())()
	if err != nil {
		t.Fatal(err)
	}
	resp, err := srv.GetProviderSchema(context.Background(), &tfprotov6.GetProviderSchemaRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, a := range resp.Provider.Block.Attributes {
		if a.Name == "token" && !a.Sensitive {
			t.Fatal("the token attribute is not sensitive")
		}
	}
}

// TestTheSchemaHasNoDeploymentResource is the "Reviewer looks for a deploy
// resource" scenario: the schema has no resource or data source that deploys,
// and the provider's documentation points to `porch score deploy`.
func TestTheSchemaHasNoDeploymentResource(t *testing.T) {
	srv, err := providerserver.NewProtocol6WithError(New("test")())()
	if err != nil {
		t.Fatal(err)
	}
	resp, err := srv.GetProviderSchema(context.Background(), &tfprotov6.GetProviderSchemaRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if e := errorText(resp.Diagnostics); e != "" {
		t.Fatal(e)
	}
	if len(resp.ResourceSchemas) < 10 {
		t.Fatalf("only %d resources in the schema", len(resp.ResourceSchemas))
	}
	names := []string{}
	for name := range resp.ResourceSchemas {
		names = append(names, name)
	}
	for name := range resp.DataSourceSchemas {
		names = append(names, name)
	}
	for name := range resp.EphemeralResourceSchemas {
		names = append(names, name)
	}
	for name := range resp.ActionSchemas {
		names = append(names, name)
	}
	for _, name := range names {
		if strings.Contains(strings.ToLower(name), "deploy") {
			t.Errorf("the schema has %q", name)
		}
	}

	pointer := "porch score deploy"
	if d := resp.Provider.Block.Description; !strings.Contains(d, pointer) {
		t.Errorf("the provider's description does not point to %q: %s", pointer, d)
	}
	docs, err := os.ReadFile("../../docs/index.md")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(docs), pointer) {
		t.Errorf("docs/index.md does not point to %q", pointer)
	}
}
