package client

import (
	"context"
	"net/http"
)

// App is an application.
type App struct {
	ID    string `json:"id"`
	Name  string `json:"name"`
	State string `json:"state,omitempty"`
}

// EnvType is an environment type.
type EnvType struct {
	ID          string `json:"id"`
	Description string `json:"description"`
}

// Env is an environment.
type Env struct {
	App        string `json:"app"`
	ID         string `json:"id"`
	Name       string `json:"name"`
	Type       string `json:"type"`
	State      string `json:"state"`
	Generation int64  `json:"generation"`
}

// CreateApp creates an application, which starts with a `development` environment.
func (c *Client) CreateApp(ctx context.Context, id, name string) (*App, error) {
	var app App
	err := c.Do(ctx, http.MethodPost, c.OrgPath("apps"), map[string]string{"id": id, "name": name}, &app)
	return &app, err
}

// App reads an application. A missing or invisible one is an *Error with status 404.
func (c *Client) App(ctx context.Context, id string) (*App, error) {
	var app App
	err := c.Do(ctx, http.MethodGet, c.OrgPath("apps", id), nil, &app)
	return &app, err
}

// RenameApp changes an application's name.
func (c *Client) RenameApp(ctx context.Context, id, name string) (*App, error) {
	var app App
	err := c.Do(ctx, http.MethodPatch, c.OrgPath("apps", id), map[string]string{"name": name}, &app)
	return &app, err
}

// CreateEnvType creates an environment type.
func (c *Client) CreateEnvType(ctx context.Context, id, description string) (*EnvType, error) {
	var t EnvType
	err := c.Do(ctx, http.MethodPost, c.OrgPath("env-types"), map[string]string{"id": id, "description": description}, &t)
	return &t, err
}

// EnvType reads an environment type.
func (c *Client) EnvType(ctx context.Context, id string) (*EnvType, error) {
	var t EnvType
	err := c.Do(ctx, http.MethodGet, c.OrgPath("env-types", id), nil, &t)
	return &t, err
}

// DescribeEnvType changes an environment type's description.
func (c *Client) DescribeEnvType(ctx context.Context, id, description string) (*EnvType, error) {
	var t EnvType
	err := c.Do(ctx, http.MethodPatch, c.OrgPath("env-types", id), map[string]string{"description": description}, &t)
	return &t, err
}

// DeleteEnvType deletes an environment type no environment uses.
func (c *Client) DeleteEnvType(ctx context.Context, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("env-types", id), nil, nil)
}

// CreateEnv creates an empty environment.
func (c *Client) CreateEnv(ctx context.Context, app, id, name, envType string) (*Env, error) {
	var env Env
	body := map[string]string{"id": id, "name": name, "type": envType}
	err := c.Do(ctx, http.MethodPost, c.OrgPath("apps", app, "envs"), body, &env)
	return &env, err
}

// Env reads an environment.
func (c *Client) Env(ctx context.Context, app, id string) (*Env, error) {
	var env Env
	err := c.Do(ctx, http.MethodGet, c.OrgPath("apps", app, "envs", id), nil, &env)
	return &env, err
}

// Envs lists an application's environments.
func (c *Client) Envs(ctx context.Context, app string) ([]Env, error) {
	var envs []Env
	err := c.Do(ctx, http.MethodGet, c.OrgPath("apps", app, "envs"), nil, &envs)
	return envs, err
}

// RenameEnv changes an environment's name.
func (c *Client) RenameEnv(ctx context.Context, app, id, name string) (*Env, error) {
	var env Env
	err := c.Do(ctx, http.MethodPatch, c.OrgPath("apps", app, "envs", id), map[string]string{"name": name}, &env)
	return &env, err
}
