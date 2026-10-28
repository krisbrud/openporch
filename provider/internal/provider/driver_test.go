package provider

import (
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/types"
)

func TestNormalizeTarget(t *testing.T) {
	for in, want := range map[string]string{
		"https://Drivers.ACME.example":            "https://drivers.acme.example/",
		"HTTPS://drivers.acme.example:443/vault":  "https://drivers.acme.example/vault",
		"https://drivers.acme.example:8443/Vault": "https://drivers.acme.example:8443/Vault",
		"https://[::1]:443":                       "https://[::1]/",
		"https://drivers.acme.example/a/b":        "https://drivers.acme.example/a/b",
		"not a url":                               "not a url",
	} {
		if got := normalizeTarget(in); got != want {
			t.Errorf("normalizeTarget(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestKeepTarget(t *testing.T) {
	m := driverModel{Target: types.StringValue("https://Host.example/a/../b")}
	// What the server wrote is saved, so a normalization Go does not know about
	// keeps the configured target.
	if !m.keepTarget("https://host.example/b", []byte(`"https://host.example/b"`)) {
		t.Error("the saved server target was not recognized")
	}
	if m.keepTarget("https://elsewhere.example/b", []byte(`"https://host.example/b"`)) {
		t.Error("a target changed outside OpenTofu was kept")
	}
	// Without a saved target, as after an import, only Go's normalization helps.
	if m.keepTarget("https://host.example/b", nil) {
		t.Error("an unsaved target was kept although Go's normalization differs")
	}
	m.Target = types.StringValue("https://Host.example")
	if !m.keepTarget("https://host.example/", nil) {
		t.Error("a normalized target was not kept")
	}
}
