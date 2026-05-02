package tofu

import (
	"fmt"
	"path/filepath"
	"strings"
)

// ResolveSource turns a Module.ModuleSource into the string that should
// appear in `module "main" { source = ... }`.
//
//   - "inline"             -> "./module" (caller writes the inline HCL)
//   - relative local path  -> absolute path under platformRoot
//   - remote (git::, github.com/, registry.*, https://, ...) -> verbatim
func ResolveSource(raw, platformRoot string) (string, error) {
	if raw == "" {
		return "", fmt.Errorf("tofu: empty module source")
	}
	if raw == "inline" {
		return "./module", nil
	}
	if !IsLocalSource(raw) {
		return raw, nil
	}
	if filepath.IsAbs(raw) {
		return filepath.Clean(raw), nil
	}
	abs, err := filepath.Abs(filepath.Join(platformRoot, raw))
	if err != nil {
		return "", fmt.Errorf("tofu: resolve local module source %q under %q: %w", raw, platformRoot, err)
	}
	return abs, nil
}

// IsLocalSource reports whether raw should be treated as a filesystem path
// relative to the platform root (or an absolute path) rather than a remote
// reference. The "inline" sentinel is not a local path.
func IsLocalSource(raw string) bool {
	if raw == "" || raw == "inline" {
		return false
	}
	if strings.HasPrefix(raw, "./") || strings.HasPrefix(raw, "../") {
		return true
	}
	if filepath.IsAbs(raw) {
		return true
	}
	if strings.Contains(raw, "::") {
		return false
	}
	if strings.HasPrefix(raw, "git@") {
		return false
	}
	if strings.HasPrefix(raw, "http://") || strings.HasPrefix(raw, "https://") {
		return false
	}
	remoteHostPrefixes := []string{
		"github.com/",
		"bitbucket.org/",
		"registry.terraform.io/",
		"app.terraform.io/",
	}
	for _, p := range remoteHostPrefixes {
		if strings.HasPrefix(raw, p) {
			return false
		}
	}
	// Bare paths like "foo/bar" are local subdirectories per Terraform's
	// own module-source parsing.
	return true
}
