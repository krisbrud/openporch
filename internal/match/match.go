// Package match implements weighted rule selection for ModuleRule and
// RunnerRule. Borrowed from Humanitec's model: each matcher field has a
// weight; if the rule names a value for a field, the context must match;
// otherwise the field is wildcard. Score = sum of weights for matched
// non-empty matchers. Highest score wins. Ties broken by rule ID
// lexicographically (deterministic).
package match

import (
	"fmt"
	"sort"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// Context carries the per-deployment / per-resource selection inputs.
type Context struct {
	ProjectID     string
	EnvID         string
	EnvTypeID     string
	ResourceType  string
	ResourceID    string
	ResourceClass string
}

const (
	weightEnvType  = 1
	weightProject  = 2
	weightEnv      = 4
	weightResource = 8
	weightClass    = 16
)

// Module returns the best-matching module ID for the given resource type and
// context. Returns ErrNoMatch if no rule applies.
func Module(rules []v1.ModuleRule, ctx Context) (string, error) {
	type scored struct {
		rule  v1.ModuleRule
		score int
	}
	var best []scored
	highest := -1
	for _, r := range rules {
		if r.ResourceType != ctx.ResourceType {
			continue
		}
		score, ok := scoreModuleRule(r, ctx)
		if !ok {
			continue
		}
		if score > highest {
			highest = score
			best = []scored{{r, score}}
		} else if score == highest {
			best = append(best, scored{r, score})
		}
	}
	if len(best) == 0 {
		return "", fmt.Errorf("%w: no module rule matched resource_type=%s ctx=%+v",
			ErrNoMatch, ctx.ResourceType, ctx)
	}
	sort.Slice(best, func(i, j int) bool { return best[i].rule.ID < best[j].rule.ID })
	return best[0].rule.ModuleID, nil
}

// Runner returns the best-matching runner ID for the given context.
func Runner(rules []v1.RunnerRule, ctx Context) (string, error) {
	type scored struct {
		rule  v1.RunnerRule
		score int
	}
	var best []scored
	highest := -1
	for _, r := range rules {
		score, ok := scoreRunnerRule(r, ctx)
		if !ok {
			continue
		}
		if score > highest {
			highest = score
			best = []scored{{r, score}}
		} else if score == highest {
			best = append(best, scored{r, score})
		}
	}
	if len(best) == 0 {
		return "", fmt.Errorf("%w: no runner rule matched ctx=%+v", ErrNoMatch, ctx)
	}
	sort.Slice(best, func(i, j int) bool { return best[i].rule.ID < best[j].rule.ID })
	return best[0].rule.RunnerID, nil
}

func scoreModuleRule(r v1.ModuleRule, c Context) (int, bool) {
	score := 0
	if r.EnvTypeID != "" {
		if r.EnvTypeID != c.EnvTypeID {
			return 0, false
		}
		score += weightEnvType
	}
	if r.ProjectID != "" {
		if r.ProjectID != c.ProjectID {
			return 0, false
		}
		score += weightProject
	}
	if r.EnvID != "" {
		if r.EnvID != c.EnvID {
			return 0, false
		}
		score += weightEnv
	}
	if r.ResourceID != "" {
		if r.ResourceID != c.ResourceID {
			return 0, false
		}
		score += weightResource
	}
	if r.ResourceClass != "" {
		if r.ResourceClass != c.ResourceClass {
			return 0, false
		}
		score += weightClass
	}
	return score, true
}

func scoreRunnerRule(r v1.RunnerRule, c Context) (int, bool) {
	score := 0
	if r.EnvTypeID != "" {
		if r.EnvTypeID != c.EnvTypeID {
			return 0, false
		}
		score += weightEnvType
	}
	if r.ProjectID != "" {
		if r.ProjectID != c.ProjectID {
			return 0, false
		}
		score += weightProject
	}
	return score, true
}

// ErrNoMatch is returned when no rule applies. Wrapped via fmt.Errorf.
var ErrNoMatch = errNoMatch{}

type errNoMatch struct{}

func (errNoMatch) Error() string { return "no rule matched" }
