// Package integrationtest contains helpers for tests that exercise real
// external tools rather than in-memory fakes.
package integrationtest

import (
	"os/exec"
	"testing"
)

// RequireDocker fails when Docker is unavailable. Integration tests are already
// guarded behind the integration build tag, so opting in means prerequisites
// must be present.
func RequireDocker(t *testing.T) {
	t.Helper()

	out, err := exec.Command("docker", "version").CombinedOutput()
	if err == nil {
		return
	}
	t.Fatalf("docker required for integration tests: %v\n%s", err, out)
}

// RequireTofu fails when OpenTofu is unavailable. Integration tests are already
// guarded behind the integration build tag, so opting in means prerequisites
// must be present.
func RequireTofu(t *testing.T) {
	t.Helper()

	_, err := exec.LookPath("tofu")
	if err == nil {
		return
	}
	t.Fatalf("tofu required for integration tests: %v", err)
}
