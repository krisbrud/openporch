package runner

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/hashicorp/terraform-exec/tfexec"
)

// LocalTofu invokes the `tofu` binary on the host. The terraform-exec
// library is CLI-compatible with OpenTofu when given the tofu binary path.
type LocalTofu struct {
	// BinaryPath is the absolute path to `tofu`. Empty = use $PATH.
	BinaryPath string

	// PluginCacheDir is set as TF_PLUGIN_CACHE_DIR when running tofu, so
	// providers are downloaded once across many resources. Empty = disabled.
	PluginCacheDir string
}

// resolveBinary finds the tofu binary path.
func (l *LocalTofu) resolveBinary() (string, error) {
	if l.BinaryPath != "" {
		return l.BinaryPath, nil
	}
	p, err := exec.LookPath("tofu")
	if err != nil {
		return "", fmt.Errorf("runner/local: tofu binary not found on PATH: %w", err)
	}
	return p, nil
}

func (l *LocalTofu) newTF(workdir, logfile string) (*tfexec.Terraform, io.Closer, error) {
	bin, err := l.resolveBinary()
	if err != nil {
		return nil, nil, err
	}
	tf, err := tfexec.NewTerraform(workdir, bin)
	if err != nil {
		return nil, nil, fmt.Errorf("runner/local: NewTerraform: %w", err)
	}
	if l.PluginCacheDir != "" {
		if err := os.MkdirAll(l.PluginCacheDir, 0o755); err != nil {
			return nil, nil, fmt.Errorf("runner/local: mkdir plugin cache: %w", err)
		}
		if err := tf.SetEnv(map[string]string{
			"TF_PLUGIN_CACHE_DIR": l.PluginCacheDir,
		}); err != nil {
			return nil, nil, fmt.Errorf("runner/local: set plugin cache env: %w", err)
		}
	}
	if err := os.MkdirAll(filepath.Dir(logfile), 0o755); err != nil {
		return nil, nil, fmt.Errorf("runner/local: mkdir log: %w", err)
	}
	lf, err := os.OpenFile(logfile, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err != nil {
		return nil, nil, fmt.Errorf("runner/local: open log: %w", err)
	}
	tf.SetStdout(lf)
	tf.SetStderr(lf)
	return tf, lf, nil
}

// Apply implements Runner.
func (l *LocalTofu) Apply(ctx context.Context, workdir, logfile string) (*Result, error) {
	tf, closer, err := l.newTF(workdir, logfile)
	if err != nil {
		return nil, err
	}
	defer closer.Close()

	if err := tf.Init(ctx, tfexec.Upgrade(false)); err != nil {
		return nil, fmt.Errorf("runner/local: tofu init in %s: %w", workdir, err)
	}
	if err := tf.Apply(ctx); err != nil {
		return nil, fmt.Errorf("runner/local: tofu apply in %s: %w", workdir, err)
	}
	raw, err := tf.Output(ctx)
	if err != nil {
		return nil, fmt.Errorf("runner/local: tofu output in %s: %w", workdir, err)
	}
	out := make(map[string]any, len(raw))
	for k, v := range raw {
		var decoded any
		if err := json.Unmarshal(v.Value, &decoded); err != nil {
			return nil, fmt.Errorf("runner/local: decode output %q: %w", k, err)
		}
		out[k] = decoded
	}
	return &Result{Outputs: out}, nil
}

// Plan implements Runner. It runs `tofu init` followed by `tofu plan -out`,
// writing the binary plan file to <workdir>/tfplan.bin and returning that path.
func (l *LocalTofu) Plan(ctx context.Context, workdir, logfile string) (string, error) {
	tf, closer, err := l.newTF(workdir, logfile)
	if err != nil {
		return "", err
	}
	defer closer.Close()

	if err := tf.Init(ctx, tfexec.Upgrade(false)); err != nil {
		return "", fmt.Errorf("runner/local: tofu init in %s: %w", workdir, err)
	}
	planPath := filepath.Join(workdir, "tfplan.bin")
	if _, err := tf.Plan(ctx, tfexec.Out(planPath)); err != nil {
		return "", fmt.Errorf("runner/local: tofu plan in %s: %w", workdir, err)
	}
	return planPath, nil
}

// Destroy implements Runner.
func (l *LocalTofu) Destroy(ctx context.Context, workdir, logfile string) error {
	tf, closer, err := l.newTF(workdir, logfile)
	if err != nil {
		return err
	}
	defer closer.Close()

	if err := tf.Init(ctx, tfexec.Upgrade(false)); err != nil {
		return fmt.Errorf("runner/local: tofu init in %s: %w", workdir, err)
	}
	if err := tf.Destroy(ctx); err != nil {
		return fmt.Errorf("runner/local: tofu destroy in %s: %w", workdir, err)
	}
	return nil
}
