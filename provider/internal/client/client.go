// Package client is a small client for the openporch HTTP API, used by the
// provider's resources.
//
// It talks to servers over verified TLS, and over plain HTTP only on loopback
// addresses. It never follows redirects, so the token cannot reach another host,
// and the token is never part of an error.
package client

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// maxBody is the largest response body the client reads.
const maxBody = 16 << 20

// ContractVersion is the API contract version this provider supports.
const ContractVersion = "1"

// contractHeader names the server's API contract version on every response.
const contractHeader = "Openporch-Contract"

// ErrInvalidToken is returned by New for a token that cannot be sent in a header.
var ErrInvalidToken = errors.New("the token contains invalid characters")

// Client sends authenticated requests to one server and organization.
type Client struct {
	base      *url.URL
	org       string
	token     string
	userAgent string
	http      *http.Client
}

// Error is an error response from the server: its HTTP status, error code (such
// as `API-001`) and message.
type Error struct {
	Status  int
	Code    string
	Message string
}

func (e *Error) Error() string {
	return fmt.Sprintf("%s (%d): %s", e.Code, e.Status, e.Message)
}

// IsNotFound reports whether err is a `404` from the server.
func IsNotFound(err error) bool {
	var apiErr *Error
	return errors.As(err, &apiErr) && apiErr.Status == http.StatusNotFound
}

// ServerURL parses and checks a server URL: `https`, or `http` on a loopback
// address, with a host and without credentials, query or fragment.
func ServerURL(raw string) (*url.URL, error) {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil {
		// The parse error repeats the input, which may hold credentials.
		return nil, errors.New("the server URL is not a valid URL")
	}
	if u.User != nil {
		return nil, errors.New("the server URL must not contain credentials")
	}
	shown := u.Redacted()
	switch {
	case u.Scheme == "https":
	case u.Scheme == "http" && IsLoopback(u):
	case u.Scheme == "http":
		return nil, fmt.Errorf("%s: plain HTTP is only allowed for loopback addresses; use https", shown)
	default:
		return nil, fmt.Errorf("%s: unsupported scheme %q", shown, u.Scheme)
	}
	if u.Host == "" {
		return nil, fmt.Errorf("%s: no host", shown)
	}
	if u.RawQuery != "" || u.Fragment != "" {
		return nil, fmt.Errorf("%s: the server URL must not have a query or fragment", shown)
	}
	u.Path = strings.TrimRight(u.Path, "/")
	u.RawPath = ""
	return u, nil
}

// IsLoopback reports whether u names this machine, where plain HTTP is allowed.
func IsLoopback(u *url.URL) bool {
	host := u.Hostname()
	if strings.EqualFold(host, "localhost") {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}

// New returns a client for the server at rawURL (see ServerURL) and organization
// org, sending token as a bearer token on every request.
func New(rawURL, org, token, userAgent string) (*Client, error) {
	base, err := ServerURL(rawURL)
	if err != nil {
		return nil, err
	}
	token = strings.TrimSpace(token)
	if strings.ContainsAny(token, "\r\n") {
		return nil, ErrInvalidToken
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	if base.Scheme == "http" {
		// Plain HTTP is only for this machine: a proxy would see the token.
		transport.Proxy = nil
	}
	return &Client{
		base:      base,
		org:       org,
		token:     token,
		userAgent: userAgent,
		http: &http.Client{
			Transport: transport,
			Timeout:   60 * time.Second,
			// A redirect could carry the token to another host.
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
	}, nil
}

// Org is the client's organization.
func (c *Client) Org() string { return c.org }

// OrgPath is `/orgs/{org}` followed by the escaped segments.
func (c *Client) OrgPath(segments ...string) string {
	var b strings.Builder
	b.WriteString("/orgs/")
	b.WriteString(url.PathEscape(c.org))
	for _, s := range segments {
		b.WriteByte('/')
		b.WriteString(url.PathEscape(s))
	}
	return b.String()
}

// Do sends a request to path (starting with `/`, segments already escaped) with
// body encoded as JSON when not nil, and decodes the response into out when not
// nil. A non-2xx response is an *Error.
func (c *Client) Do(ctx context.Context, method, path string, body, out any) error {
	return c.DoWithHeaders(ctx, method, path, nil, body, out)
}

// DoWithHeaders is Do with extra request headers.
func (c *Client) DoWithHeaders(ctx context.Context, method, path string, headers map[string]string, body, out any) error {
	if !strings.HasPrefix(path, "/") {
		return fmt.Errorf("request path %q must start with /", path)
	}
	var reader io.Reader
	if body != nil {
		data, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("encoding the request: %w", err)
		}
		reader = bytes.NewReader(data)
	}
	target := c.base.String() + path
	req, err := http.NewRequestWithContext(ctx, method, target, reader)
	if err != nil {
		return fmt.Errorf("%s %s: %w", method, path, err)
	}
	req.Header.Set("Accept", "application/json")
	// A server of another contract refuses the request before acting on it.
	req.Header.Set(contractHeader, ContractVersion)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	for name, value := range headers {
		req.Header.Set(name, value)
	}
	if c.userAgent != "" {
		req.Header.Set("User-Agent", c.userAgent)
	}
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	res, err := c.http.Do(req)
	if err != nil {
		return fmt.Errorf("%s %s: %w", method, path, err)
	}
	defer res.Body.Close()
	// Servers from before the header serve contract 1.
	if contract := res.Header.Get(contractHeader); contract != "" && contract != ContractVersion {
		if len(contract) > 32 {
			contract = contract[:32]
		}
		return fmt.Errorf("the server serves API contract %q, but this provider supports contract %s: "+
			"use a provider from the same openporch release as the server", contract, ContractVersion)
	}
	data, err := io.ReadAll(io.LimitReader(res.Body, maxBody+1))
	if err != nil {
		return fmt.Errorf("%s %s: reading the response: %w", method, path, err)
	}
	if len(data) > maxBody {
		return fmt.Errorf("%s %s: the response is larger than %d bytes", method, path, maxBody)
	}
	if res.StatusCode >= 300 && res.StatusCode <= 399 {
		where := "elsewhere"
		if loc, err := res.Location(); err == nil {
			where = "to " + loc.Scheme + "://" + loc.Host
		}
		return &Error{
			Status: res.StatusCode,
			Code:   "HTTP",
			Message: "the server redirected " + where + ", and redirects are not followed; " +
				"check the scheme and path of the server URL",
		}
	}
	if res.StatusCode < 200 || res.StatusCode > 299 {
		return apiError(res.StatusCode, data)
	}
	if out == nil {
		return nil
	}
	if len(data) == 0 {
		return fmt.Errorf("%s %s: the response is empty", method, path)
	}
	dec := json.NewDecoder(bytes.NewReader(data))
	// Numbers in free-form JSON keep their exact text.
	dec.UseNumber()
	if err := dec.Decode(out); err != nil {
		return fmt.Errorf("%s %s: decoding the response: %w", method, path, err)
	}
	return nil
}

func apiError(status int, data []byte) *Error {
	var body struct {
		Error   string `json:"error"`
		Message string `json:"message"`
	}
	if json.Unmarshal(data, &body) == nil && body.Error != "" {
		return &Error{Status: status, Code: body.Error, Message: body.Message}
	}
	message := http.StatusText(status)
	if message == "" {
		message = "request failed"
	}
	return &Error{Status: status, Code: "HTTP", Message: message}
}

// CurrentUser is the caller of a request, as `GET /orgs/{org}/current-user`
// returns it.
type CurrentUser struct {
	ID   string `json:"id"`
	Type string `json:"type"`
	Name string `json:"name"`
	Role string `json:"role"`
}

// CurrentUser returns who the token belongs to. It is how the provider checks its
// configuration.
func (c *Client) CurrentUser(ctx context.Context) (*CurrentUser, error) {
	var user CurrentUser
	if err := c.Do(ctx, http.MethodGet, c.OrgPath("current-user"), nil, &user); err != nil {
		return nil, err
	}
	return &user, nil
}
