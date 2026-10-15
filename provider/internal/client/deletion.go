package client

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"sort"
	"strings"
	"time"
)

// Ack acknowledges one change a deletion makes, written
// `DESCRIPTOR=ACTION[@FINGERPRINT]` as `porch` prints it.
type Ack struct {
	Descriptor  string
	Action      string
	Fingerprint string
}

// ParseAck parses `DESCRIPTOR=ACTION[@FINGERPRINT]`.
func ParseAck(s string) (Ack, error) {
	named, fingerprint, hasFingerprint := strings.Cut(s, "@")
	if hasFingerprint && fingerprint == "" {
		return Ack{}, fmt.Errorf("%q: expected a fingerprint after `@`", s)
	}
	i := strings.LastIndex(named, "=")
	if i <= 0 || i == len(named)-1 {
		return Ack{}, fmt.Errorf("%q: expected DESCRIPTOR=ACTION", s)
	}
	return Ack{Descriptor: named[:i], Action: named[i+1:], Fingerprint: fingerprint}, nil
}

func (a Ack) String() string {
	if a.Fingerprint != "" {
		return a.Descriptor + "=" + a.Action + "@" + a.Fingerprint
	}
	return a.Descriptor + "=" + a.Action
}

// MissingAcksError is a deletion refused before anything changed because changes
// it would make are not acknowledged.
type MissingAcksError struct {
	// What is being deleted, such as `application shop`.
	What string
	// Missing are the acknowledgements to add, in the form they are configured.
	Missing []string
}

func (e *MissingAcksError) Error() string {
	var b strings.Builder
	fmt.Fprintf(&b, "deleting %s deletes resources that have not been acknowledged, so nothing was deleted. "+
		"Add these to its destroy_acknowledgements, apply, and delete it again:", e.What)
	for _, m := range e.Missing {
		fmt.Fprintf(&b, "\n  %q,", m)
	}
	return b.String()
}

type planEntry struct {
	Descriptor     string `json:"descriptor"`
	Action         string `json:"action"`
	PermitRequired bool   `json:"permit_required"`
	DriverPlan     *struct {
		Fingerprint string `json:"fingerprint"`
	} `json:"driver_plan"`
}

func (e planEntry) ack() Ack {
	a := Ack{Descriptor: e.Descriptor, Action: e.Action}
	if e.DriverPlan != nil {
		a.Fingerprint = e.DriverPlan.Fingerprint
	}
	return a
}

type plan struct {
	ID             string      `json:"id"`
	BaseGeneration int64       `json:"base_generation"`
	Resources      []planEntry `json:"resources"`
}

type permitAsk struct {
	Descriptor  string  `json:"descriptor"`
	Action      string  `json:"action"`
	Fingerprint *string `json:"fingerprint"`
}

// envDeletion is a previewed environment deletion and the permits it asks for.
type envDeletion struct {
	app, env string
	plan     plan
	asks     []permitAsk
}

// previewEnvDeletion previews deleting app/env and matches acks against the
// changes that need a permit. It returns the acknowledgements still missing.
func (c *Client) previewEnvDeletion(ctx context.Context, app string, env *Env, acks []Ack) (*envDeletion, []Ack, error) {
	var p plan
	body := map[string]any{"kind": "delete_env", "expected_generation": env.Generation}
	if err := c.Do(ctx, http.MethodPost, c.OrgPath("apps", app, "envs", env.ID, "plans"), body, &p); err != nil {
		return nil, nil, fmt.Errorf("previewing the deletion of environment %s/%s: %w", app, env.ID, err)
	}
	d := &envDeletion{app: app, env: env.ID, plan: p}
	var missing []Ack
	for _, entry := range p.Resources {
		if !entry.PermitRequired {
			continue
		}
		want := entry.ack()
		if !containsAck(acks, want) {
			missing = append(missing, want)
			continue
		}
		ask := permitAsk{Descriptor: want.Descriptor, Action: want.Action}
		if want.Fingerprint != "" {
			fp := want.Fingerprint
			ask.Fingerprint = &fp
		}
		d.asks = append(d.asks, ask)
	}
	return d, missing, nil
}

func containsAck(acks []Ack, want Ack) bool {
	for _, a := range acks {
		if a == want {
			return true
		}
	}
	return false
}

// issue issues the permits d asks for and returns their IDs.
func (c *Client) issue(ctx context.Context, d *envDeletion) ([]string, error) {
	if len(d.asks) == 0 {
		return []string{}, nil
	}
	var issued struct {
		Permits []struct {
			ID string `json:"id"`
		} `json:"permits"`
	}
	path := c.OrgPath("apps", d.app, "envs", d.env, "plans", d.plan.ID, "permits")
	if err := c.Do(ctx, http.MethodPost, path, map[string]any{"permits": d.asks}, &issued); err != nil {
		return nil, fmt.Errorf("issuing permits for deleting environment %s/%s: %w", d.app, d.env, err)
	}
	ids := make([]string, 0, len(issued.Permits))
	for _, p := range issued.Permits {
		ids = append(ids, p.ID)
	}
	return ids, nil
}

// pollInterval is how often a deletion is checked while it runs.
var pollInterval = time.Second

// DeleteEnv deletes environment app/env and every resource it owns, once acks
// acknowledge each change that needs a permit, and waits until it is gone.
// Without them it returns a *MissingAcksError and deletes nothing.
func (c *Client) DeleteEnv(ctx context.Context, app, id string, acks []Ack, comment string) error {
	env, err := c.Env(ctx, app, id)
	if IsNotFound(err) {
		return nil
	}
	if err != nil {
		return err
	}
	if env.State != "deleting" {
		d, missing, err := c.previewEnvDeletion(ctx, app, env, acks)
		if err != nil {
			return err
		}
		if len(missing) > 0 {
			return &MissingAcksError{What: "environment " + app + "/" + id, Missing: ackStrings("", missing)}
		}
		permits, err := c.issue(ctx, d)
		if err != nil {
			return err
		}
		body := map[string]any{
			"expected_generation": d.plan.BaseGeneration,
			"permits":             permits,
			"comment":             comment,
		}
		// The key returns the same deletion to a retry of this request.
		headers := map[string]string{"Idempotency-Key": "openporch-provider-" + d.plan.ID}
		err = c.DoWithHeaders(ctx, http.MethodDelete, c.OrgPath("apps", app, "envs", id), headers, body, nil)
		if err != nil && !IsNotFound(err) {
			return fmt.Errorf("deleting environment %s/%s: %w", app, id, err)
		}
	}
	return c.waitGone(ctx, "environment "+app+"/"+id, func() (string, error) {
		env, err := c.Env(ctx, app, id)
		if err != nil {
			return "", err
		}
		return env.State, nil
	})
}

// DeleteApp deletes application app with every environment and the resources
// they own, once acks (by environment) acknowledge each change that needs a
// permit, and waits until it is gone. Without them it returns a
// *MissingAcksError and deletes nothing.
func (c *Client) DeleteApp(ctx context.Context, app string, acks map[string][]Ack, comment string) error {
	if _, err := c.App(ctx, app); IsNotFound(err) {
		return nil
	} else if err != nil {
		return err
	}
	envs, err := c.Envs(ctx, app)
	if err != nil {
		return err
	}
	var deletions []*envDeletion
	var missing []string
	for i := range envs {
		env := &envs[i]
		if env.State == "deleting" {
			continue
		}
		d, m, err := c.previewEnvDeletion(ctx, app, env, acks[env.ID])
		if err != nil {
			return err
		}
		deletions = append(deletions, d)
		missing = append(missing, ackStrings(env.ID+":", m)...)
	}
	if len(missing) > 0 {
		sort.Strings(missing)
		return &MissingAcksError{What: "application " + app, Missing: missing}
	}
	permits := []string{}
	for _, d := range deletions {
		ids, err := c.issue(ctx, d)
		if err != nil {
			return err
		}
		permits = append(permits, ids...)
	}
	body := map[string]any{"permits": permits, "comment": comment}
	if err := c.Do(ctx, http.MethodDelete, c.OrgPath("apps", app), body, nil); err != nil && !IsNotFound(err) {
		return fmt.Errorf("deleting application %s: %w", app, err)
	}
	return c.waitGone(ctx, "application "+app, func() (string, error) {
		envs, err := c.Envs(ctx, app)
		if err != nil {
			return "", err
		}
		for _, e := range envs {
			if e.State == "deletion_failed" {
				return "deletion_failed", nil
			}
		}
		return "deleting", nil
	})
}

func ackStrings(prefix string, acks []Ack) []string {
	out := make([]string, 0, len(acks))
	for _, a := range acks {
		out = append(out, prefix+a.String())
	}
	return out
}

// waitGone polls state until it reports 404, and fails when the deletion failed.
func (c *Client) waitGone(ctx context.Context, what string, state func() (string, error)) error {
	for {
		s, err := state()
		if IsNotFound(err) {
			return nil
		}
		if err != nil {
			return err
		}
		if s == "deletion_failed" {
			return fmt.Errorf("deleting %s failed: a resource could not be deleted. `porch resources graph` shows "+
				"which and why; fix the cause and delete again", what)
		}
		select {
		case <-ctx.Done():
			return fmt.Errorf("waiting for %s to be deleted: %w", what, errors.Join(ctx.Err()))
		case <-time.After(pollInterval):
		}
	}
}
