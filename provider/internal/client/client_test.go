package client

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestServerURLsNeedHTTPSExceptOnLoopback(t *testing.T) {
	for _, ok := range []string{
		"https://porch.example.com",
		"https://porch.example.com/base/",
		"http://127.0.0.1:8080",
		"http://localhost:8080",
		"http://[::1]:8080",
	} {
		if _, err := ServerURL(ok); err != nil {
			t.Errorf("ServerURL(%q): %v", ok, err)
		}
	}
	for _, bad := range []string{
		"http://porch.example.com",
		"ftp://porch.example.com",
		"https://user:secret@porch.example.com",
		"https://porch.example.com/?q=1",
		"https://",
		"porch.example.com",
	} {
		if _, err := ServerURL(bad); err == nil {
			t.Errorf("ServerURL(%q) was accepted", bad)
		}
	}
	for _, withSecret := range []string{
		"http://user:s3cret@porch.example.com",
		"https://user:s3cret@porch.example.com/%zz",
	} {
		_, err := ServerURL(withSecret)
		if err == nil || strings.Contains(err.Error(), "s3cret") {
			t.Errorf("ServerURL(%q): %v", withSecret, err)
		}
	}
	u, err := ServerURL("https://porch.example.com/base/")
	if err != nil || u.String() != "https://porch.example.com/base" {
		t.Errorf("ServerURL kept a trailing slash: %v, %v", u, err)
	}
}

func TestRequestsCarryTheTokenAndErrorsDoNot(t *testing.T) {
	const token = "opt_secret-value"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer "+token {
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":"AUTH-001","message":"no valid token or session"}`))
			return
		}
		if r.URL.EscapedPath() != "/orgs/acme/current-user" {
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte(`{"error":"API-001","message":"not found"}`))
			return
		}
		_, _ = w.Write([]byte(`{"id":"ci","type":"service","name":"CI","role":"manager","roles":{}}`))
	}))
	defer srv.Close()

	c, err := New(srv.URL, "acme", token, "test")
	if err != nil {
		t.Fatal(err)
	}
	user, err := c.CurrentUser(context.Background())
	if err != nil || user.ID != "ci" || user.Role != "manager" {
		t.Fatalf("CurrentUser: %+v, %v", user, err)
	}

	wrong, err := New(srv.URL, "acme", "opt_wrong-value", "test")
	if err != nil {
		t.Fatal(err)
	}
	_, err = wrong.CurrentUser(context.Background())
	var apiErr *Error
	if err == nil || !strings.Contains(err.Error(), "AUTH-001") {
		t.Fatalf("expected AUTH-001, got %v", err)
	}
	if strings.Contains(err.Error(), "opt_wrong-value") {
		t.Fatalf("the error shows the token: %v", err)
	}
	if !errors.As(err, &apiErr) || apiErr.Status != http.StatusUnauthorized {
		t.Fatalf("expected a 401 *Error, got %#v", err)
	}

	err = c.Do(context.Background(), http.MethodGet, c.OrgPath("apps", "missing"), nil, nil)
	if !IsNotFound(err) {
		t.Fatalf("expected a 404, got %v", err)
	}
}

func TestRedirectsAreNotFollowed(t *testing.T) {
	var leaked bool
	other := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		leaked = r.Header.Get("Authorization") != ""
	}))
	defer other.Close()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, other.URL+r.URL.Path, http.StatusFound)
	}))
	defer srv.Close()

	c, err := New(srv.URL, "acme", "opt_secret-value", "test")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.CurrentUser(context.Background()); err == nil || !strings.Contains(err.Error(), "redirects are not followed") {
		t.Fatalf("expected a redirect error, got %v", err)
	}
	if leaked {
		t.Fatal("the token followed a redirect")
	}
}

func TestOrgPathsEscapeSegments(t *testing.T) {
	c, err := New("http://127.0.0.1:8080", "acme", "t", "test")
	if err != nil {
		t.Fatal(err)
	}
	if got := c.OrgPath("resources", "types", "acme/queue"); got != "/orgs/acme/resources/types/acme%2Fqueue" {
		t.Fatalf("OrgPath: %s", got)
	}
}
