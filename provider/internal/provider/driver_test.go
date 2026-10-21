package provider

import "testing"

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
