package match

import (
	"errors"
	"testing"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

func TestModule_emptyRuleIsCatchAll(t *testing.T) {
	t.Parallel()
	rules := []v1.ModuleRule{
		{ID: "catchall", ResourceType: "postgres", ModuleID: "postgres-default"},
	}
	got, err := Module(rules, Context{ResourceType: "postgres", EnvTypeID: "any"})
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	if got != "postgres-default" {
		t.Fatalf("got %q", got)
	}
}

func TestModule_higherSpecificityWins(t *testing.T) {
	t.Parallel()
	rules := []v1.ModuleRule{
		{ID: "catchall", ResourceType: "postgres", ModuleID: "default"},
		{ID: "byEnvType", ResourceType: "postgres", ModuleID: "by-envtype", EnvTypeID: "production"},
		{ID: "byProject", ResourceType: "postgres", ModuleID: "by-project", ProjectID: "demo"},
	}
	// byProject has weight 2, byEnvType has weight 1, catchall has 0.
	got, err := Module(rules, Context{ResourceType: "postgres", ProjectID: "demo", EnvTypeID: "production"})
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	if got != "by-project" {
		t.Fatalf("got %q want by-project", got)
	}
}

func TestModule_resourceClassDominates(t *testing.T) {
	t.Parallel()
	rules := []v1.ModuleRule{
		{ID: "byEverything", ResourceType: "postgres", ModuleID: "everything",
			EnvTypeID: "production", ProjectID: "demo", EnvID: "prod", ResourceID: "db"},
		{ID: "byClass", ResourceType: "postgres", ModuleID: "class", ResourceClass: "premium"},
	}
	// byEverything = 1+2+4+8 = 15. byClass = 16. byClass wins.
	got, err := Module(rules, Context{
		ResourceType: "postgres", EnvTypeID: "production", ProjectID: "demo",
		EnvID: "prod", ResourceID: "db", ResourceClass: "premium",
	})
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	if got != "class" {
		t.Fatalf("got %q want class", got)
	}
}

func TestModule_nonMatchingValueDisqualifies(t *testing.T) {
	t.Parallel()
	rules := []v1.ModuleRule{
		{ID: "specific", ResourceType: "postgres", ModuleID: "specific", EnvTypeID: "production"},
	}
	_, err := Module(rules, Context{ResourceType: "postgres", EnvTypeID: "local"})
	if !errors.Is(err, ErrNoMatch) {
		t.Fatalf("want ErrNoMatch, got %v", err)
	}
}

func TestModule_resourceTypeFilters(t *testing.T) {
	t.Parallel()
	rules := []v1.ModuleRule{
		{ID: "wrongType", ResourceType: "redis", ModuleID: "redis"},
	}
	_, err := Module(rules, Context{ResourceType: "postgres"})
	if !errors.Is(err, ErrNoMatch) {
		t.Fatalf("want ErrNoMatch, got %v", err)
	}
}

func TestModule_tieBreakByID(t *testing.T) {
	t.Parallel()
	rules := []v1.ModuleRule{
		{ID: "z-rule", ResourceType: "postgres", ModuleID: "z", EnvTypeID: "prod"},
		{ID: "a-rule", ResourceType: "postgres", ModuleID: "a", EnvTypeID: "prod"},
	}
	got, err := Module(rules, Context{ResourceType: "postgres", EnvTypeID: "prod"})
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	if got != "a" {
		t.Fatalf("got %q want a (lex tie-break)", got)
	}
}

func TestRunner_specificityOrder(t *testing.T) {
	t.Parallel()
	rules := []v1.RunnerRule{
		{ID: "any", RunnerID: "r-any"},
		{ID: "byEnvType", RunnerID: "r-envtype", EnvTypeID: "production"},
		{ID: "byProject", RunnerID: "r-project", ProjectID: "demo"},
		{ID: "both", RunnerID: "r-both", ProjectID: "demo", EnvTypeID: "production"},
	}
	got, err := Runner(rules, Context{ProjectID: "demo", EnvTypeID: "production"})
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	if got != "r-both" {
		t.Fatalf("got %q want r-both", got)
	}
}
