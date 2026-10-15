package client

import (
	"context"
	"fmt"
	"net/http"
)

// User is an organization member: a person or a service user.
type User struct {
	ID   string `json:"id"`
	Type string `json:"type"`
	Name string `json:"name"`
	Role string `json:"role"`
}

// CreateServiceUser creates a service user.
func (c *Client) CreateServiceUser(ctx context.Context, id, name, role string) (*User, error) {
	var u User
	body := map[string]string{"id": id, "type": "service", "name": name, "role": role}
	err := c.Do(ctx, http.MethodPost, c.OrgPath("users"), body, &u)
	return &u, err
}

// User reads a member.
func (c *Client) User(ctx context.Context, id string) (*User, error) {
	var u User
	err := c.Do(ctx, http.MethodGet, c.OrgPath("users", id), nil, &u)
	return &u, err
}

// UpdateUser changes a member's role or name; empty arguments are left as they are.
func (c *Client) UpdateUser(ctx context.Context, id, name, role string) (*User, error) {
	body := map[string]string{}
	if name != "" {
		body["name"] = name
	}
	if role != "" {
		body["role"] = role
	}
	var u User
	err := c.Do(ctx, http.MethodPatch, c.OrgPath("users", id), body, &u)
	return &u, err
}

// DeleteUser removes a member with their tokens and roles.
func (c *Client) DeleteUser(ctx context.Context, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("users", id), nil, nil)
}

// Token describes an API token. Its value is only in CreatedToken.
type Token struct {
	ID          string  `json:"id"`
	Description *string `json:"description"`
	ExpiresAt   *int64  `json:"expires_at"`
	RevokedAt   *int64  `json:"revoked_at"`
}

// CreatedToken is a token as its creation returns it, with its value.
type CreatedToken struct {
	Token
	Value string `json:"token"`
}

// CreateToken issues a token for user. expiresAt is in Unix milliseconds.
func (c *Client) CreateToken(ctx context.Context, user, id string, description *string, expiresAt *int64) (*CreatedToken, error) {
	body := map[string]any{"id": id}
	if description != nil {
		body["description"] = *description
	}
	if expiresAt != nil {
		body["expires_at"] = *expiresAt
	}
	var t CreatedToken
	err := c.Do(ctx, http.MethodPost, c.OrgPath("users", user, "tokens"), body, &t)
	return &t, err
}

// Token reads a token's description, without its value.
func (c *Client) Token(ctx context.Context, user, id string) (*Token, error) {
	var t Token
	err := c.Do(ctx, http.MethodGet, c.OrgPath("users", user, "tokens", id), nil, &t)
	return &t, err
}

// RevokeToken revokes a token.
func (c *Client) RevokeToken(ctx context.Context, user, id string) error {
	return c.Do(ctx, http.MethodDelete, c.OrgPath("users", user, "tokens", id), nil, nil)
}

// RoleGrant is a member's role on an application or environment type.
type RoleGrant struct {
	User string `json:"user"`
	Role string `json:"role"`
}

// rolesPath is the path of the role grants on an application (scope "apps") or
// environment type (scope "env-types").
func (c *Client) rolesPath(scope, id string, user ...string) string {
	return c.OrgPath(append([]string{scope, id, "users"}, user...)...)
}

// Role reads user's role on an application or environment type; scope is "apps"
// or "env-types". A missing grant is an *Error with status 404.
func (c *Client) Role(ctx context.Context, scope, id, user string) (*RoleGrant, error) {
	var grants []RoleGrant
	if err := c.Do(ctx, http.MethodGet, c.rolesPath(scope, id), nil, &grants); err != nil {
		return nil, err
	}
	for i := range grants {
		if grants[i].User == user {
			return &grants[i], nil
		}
	}
	return nil, &Error{Status: http.StatusNotFound, Code: "API-001",
		Message: fmt.Sprintf("`%s` has no role on `%s`", user, id)}
}

// GrantRole gives user a role on an application or environment type.
func (c *Client) GrantRole(ctx context.Context, scope, id, user, role string) (*RoleGrant, error) {
	var g RoleGrant
	err := c.Do(ctx, http.MethodPost, c.rolesPath(scope, id), map[string]string{"id": user, "role": role}, &g)
	return &g, err
}

// ChangeAppRole changes user's role on an application.
func (c *Client) ChangeAppRole(ctx context.Context, app, user, role string) (*RoleGrant, error) {
	var g RoleGrant
	err := c.Do(ctx, http.MethodPatch, c.rolesPath("apps", app, user), map[string]string{"role": role}, &g)
	return &g, err
}

// RevokeRole removes user's role on an application or environment type.
func (c *Client) RevokeRole(ctx context.Context, scope, id, user string) error {
	return c.Do(ctx, http.MethodDelete, c.rolesPath(scope, id, user), nil, nil)
}
