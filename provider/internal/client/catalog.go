package client

import (
	"context"
	"fmt"
	"net/http"
)

// SecretRef is a reference to a secret: its store, path in the store and version.
// Version is empty for "the current version".
type SecretRef struct {
	Store   string `json:"store"`
	Ref     string `json:"ref"`
	Version string `json:"version,omitempty"`
}

// SecretWrite is a secret input: a reference, or a value to write to the primary
// store.
type SecretWrite struct {
	*SecretRef
	Value *string `json:"value,omitempty"`
}

// Class is a resource class.
type Class struct {
	ID           string `json:"id"`
	ResourceType string `json:"resource_type"`
	Description  string `json:"description"`
}

// CreateClass adds a class to a resource type.
func (c *Client) CreateClass(ctx context.Context, resourceType, id, description string) (*Class, error) {
	var class Class
	body := map[string]string{"id": id, "description": description}
	err := c.Do(ctx, http.MethodPost, c.OrgPath("resources", "types", resourceType, "classes"), body, &class)
	return &class, err
}

// Class reads a class; a missing one is an *Error with status 404.
func (c *Client) Class(ctx context.Context, resourceType, id string) (*Class, error) {
	var classes []Class
	if err := c.Do(ctx, http.MethodGet, c.OrgPath("resources", "types", resourceType, "classes"), nil, &classes); err != nil {
		return nil, err
	}
	for i := range classes {
		if classes[i].ID == id {
			return &classes[i], nil
		}
	}
	return nil, &Error{Status: http.StatusNotFound, Code: "API-001",
		Message: fmt.Sprintf("resource type `%s` has no class `%s`", resourceType, id)}
}

// DeleteClass deletes a class.
func (c *Client) DeleteClass(ctx context.Context, resourceType, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("resources", "types", resourceType, "classes", id), nil, nil)
}

// SharedValue is an application or environment value: plain, or a secret reference.
type SharedValue struct {
	Key         string     `json:"key"`
	Description string     `json:"description"`
	Value       *string    `json:"value,omitempty"`
	SecretRef   *SecretRef `json:"secret_ref,omitempty"`
}

// ValueWrite is the body that creates or replaces a value.
type ValueWrite struct {
	Key         string       `json:"key,omitempty"`
	Description string       `json:"description"`
	Value       *string      `json:"value,omitempty"`
	SecretRef   *SecretWrite `json:"secret_ref,omitempty"`
}

func (c *Client) valuesPath(app, env string, key ...string) string {
	segments := []string{"apps", app}
	if env != "" {
		segments = append(segments, "envs", env)
	}
	segments = append(segments, "values")
	return c.OrgPath(append(segments, key...)...)
}

// CreateValue creates a value of app, or of app/env when env is not empty.
func (c *Client) CreateValue(ctx context.Context, app, env string, body ValueWrite) (*SharedValue, error) {
	var v SharedValue
	err := c.Do(ctx, http.MethodPost, c.valuesPath(app, env), body, &v)
	return &v, err
}

// ReplaceValue replaces a value.
func (c *Client) ReplaceValue(ctx context.Context, app, env, key string, body ValueWrite) (*SharedValue, error) {
	var v SharedValue
	body.Key = ""
	err := c.Do(ctx, http.MethodPut, c.valuesPath(app, env, key), body, &v)
	return &v, err
}

// Value reads a value.
func (c *Client) Value(ctx context.Context, app, env, key string) (*SharedValue, error) {
	var v SharedValue
	err := c.Do(ctx, http.MethodGet, c.valuesPath(app, env, key), nil, &v)
	return &v, err
}

// DeleteValue deletes a value.
func (c *Client) DeleteValue(ctx context.Context, app, env, key string) error {
	return c.Do(ctx, http.MethodDelete, c.valuesPath(app, env, key), nil, nil)
}

// Account is a cloud account.
type Account struct {
	ID         string               `json:"id"`
	Name       string               `json:"name"`
	Type       string               `json:"type"`
	Config     map[string]any       `json:"config"`
	SecretRefs map[string]SecretRef `json:"secret_refs"`
}

// AccountWrite is the body that creates or replaces a cloud account.
type AccountWrite struct {
	ID         string                 `json:"id,omitempty"`
	Name       string                 `json:"name,omitempty"`
	Type       string                 `json:"type"`
	Config     map[string]any         `json:"config"`
	SecretRefs map[string]SecretWrite `json:"secret_refs"`
}

// CreateAccount creates a cloud account.
func (c *Client) CreateAccount(ctx context.Context, body AccountWrite) (*Account, error) {
	var a Account
	err := c.Do(ctx, http.MethodPost, c.OrgPath("resources", "accounts"), body, &a)
	return &a, err
}

// ReplaceAccount replaces a cloud account.
func (c *Client) ReplaceAccount(ctx context.Context, id string, body AccountWrite) (*Account, error) {
	var a Account
	body.ID = ""
	err := c.Do(ctx, http.MethodPut, c.OrgPath("resources", "accounts", id), body, &a)
	return &a, err
}

// Account reads a cloud account.
func (c *Client) Account(ctx context.Context, id string) (*Account, error) {
	var a Account
	err := c.Do(ctx, http.MethodGet, c.OrgPath("resources", "accounts", id), nil, &a)
	return &a, err
}

// DeleteAccount deletes a cloud account nothing uses.
func (c *Client) DeleteAccount(ctx context.Context, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("resources", "accounts", id), nil, nil)
}
