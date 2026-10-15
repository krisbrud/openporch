package client

import (
	"context"
	"fmt"
	"net/http"
)

// DriverInputs are a definition's driver inputs: plain values, secret templates
// and secret references, each a JSON object. A secret written by the server comes
// back under SecretRefs.
type DriverInputs struct {
	Values     map[string]any `json:"values"`
	Secrets    map[string]any `json:"secrets,omitempty"`
	SecretRefs map[string]any `json:"secret_refs,omitempty"`
}

// Criterion is a matching criterion of a definition. Empty fields match anything.
type Criterion struct {
	ID      string `json:"id,omitempty"`
	EnvType string `json:"env_type,omitempty"`
	AppID   string `json:"app_id,omitempty"`
	EnvID   string `json:"env_id,omitempty"`
	ResID   string `json:"res_id,omitempty"`
	Class   string `json:"class,omitempty"`
}

// Definition is a resource definition.
type Definition struct {
	ID            string         `json:"id"`
	Name          string         `json:"name"`
	Type          string         `json:"type"`
	DriverType    string         `json:"driver_type"`
	DriverAccount string         `json:"driver_account,omitempty"`
	DriverInputs  DriverInputs   `json:"driver_inputs"`
	Provision     map[string]any `json:"provision,omitempty"`
	Criteria      []Criterion    `json:"criteria"`
}

// DefinitionWrite is the body that creates (with ID and Type) or replaces a
// definition.
type DefinitionWrite struct {
	ID            string         `json:"id,omitempty"`
	Name          string         `json:"name"`
	Type          string         `json:"type,omitempty"`
	DriverType    string         `json:"driver_type"`
	DriverAccount string         `json:"driver_account,omitempty"`
	DriverInputs  DriverInputs   `json:"driver_inputs"`
	Provision     map[string]any `json:"provision,omitempty"`
}

// CreateDefinition creates a definition without criteria.
func (c *Client) CreateDefinition(ctx context.Context, body DefinitionWrite) (*Definition, error) {
	var d Definition
	err := c.Do(ctx, http.MethodPost, c.OrgPath("resources", "defs"), body, &d)
	return &d, err
}

// ReplaceDefinition replaces a definition's name, driver, account, inputs and
// co-provisioning; its type and criteria stay.
func (c *Client) ReplaceDefinition(ctx context.Context, id string, body DefinitionWrite) (*Definition, error) {
	body.ID, body.Type = "", ""
	var d Definition
	err := c.Do(ctx, http.MethodPut, c.OrgPath("resources", "defs", id), body, &d)
	return &d, err
}

// Definition reads a definition.
func (c *Client) Definition(ctx context.Context, id string) (*Definition, error) {
	var d Definition
	err := c.Do(ctx, http.MethodGet, c.OrgPath("resources", "defs", id), nil, &d)
	return &d, err
}

// DeleteDefinition deletes a definition.
func (c *Client) DeleteDefinition(ctx context.Context, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("resources", "defs", id), nil, nil)
}

// AddCriterion adds a criterion to a definition and returns it with its ID.
func (c *Client) AddCriterion(ctx context.Context, def string, criterion Criterion) (*Criterion, error) {
	criterion.ID = ""
	var out Criterion
	err := c.Do(ctx, http.MethodPost, c.OrgPath("resources", "defs", def, "criteria"), criterion, &out)
	return &out, err
}

// Criterion reads a criterion of a definition; a missing one is an *Error with
// status 404.
func (c *Client) Criterion(ctx context.Context, def, id string) (*Criterion, error) {
	d, err := c.Definition(ctx, def)
	if err != nil {
		return nil, err
	}
	for i := range d.Criteria {
		if d.Criteria[i].ID == id {
			return &d.Criteria[i], nil
		}
	}
	return nil, &Error{Status: http.StatusNotFound, Code: "API-001",
		Message: fmt.Sprintf("resource definition `%s` has no criterion `%s`", def, id)}
}

// DeleteCriterion removes a criterion.
func (c *Client) DeleteCriterion(ctx context.Context, def, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("resources", "defs", def, "criteria", id), nil, nil)
}
