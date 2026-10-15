package provider

import (
	"context"
	"encoding/json"
	"fmt"
	"math/big"
	"reflect"

	"github.com/hashicorp/terraform-plugin-framework/types"
	"github.com/hashicorp/terraform-plugin-framework/types/basetypes"
	"github.com/hashicorp/terraform-plugin-go/tftypes"
)

// dynamicToJSON converts a configured dynamic value, such as `{ region = "eu-north-1" }`,
// to the JSON value the API takes. Unknown values are an error.
func dynamicToJSON(ctx context.Context, v types.Dynamic) (any, error) {
	if v.IsNull() || v.IsUnderlyingValueNull() {
		return nil, nil
	}
	if v.IsUnknown() || v.IsUnderlyingValueUnknown() {
		return nil, fmt.Errorf("the value is not known until apply")
	}
	tf, err := v.ToTerraformValue(ctx)
	if err != nil {
		return nil, err
	}
	return tfToJSON(tf)
}

func tfToJSON(v tftypes.Value) (any, error) {
	if !v.IsKnown() {
		return nil, fmt.Errorf("the value is not known until apply")
	}
	if v.IsNull() {
		return nil, nil
	}
	t := v.Type()
	switch {
	case t.Is(tftypes.String):
		var s string
		err := v.As(&s)
		return s, err
	case t.Is(tftypes.Bool):
		var b bool
		err := v.As(&b)
		return b, err
	case t.Is(tftypes.Number):
		var n big.Float
		if err := v.As(&n); err != nil {
			return nil, err
		}
		return json.Number(n.Text('g', -1)), nil
	case t.Is(tftypes.Object{}), t.Is(tftypes.Map{}):
		var m map[string]tftypes.Value
		if err := v.As(&m); err != nil {
			return nil, err
		}
		out := make(map[string]any, len(m))
		for k, e := range m {
			j, err := tfToJSON(e)
			if err != nil {
				return nil, err
			}
			out[k] = j
		}
		return out, nil
	case t.Is(tftypes.List{}), t.Is(tftypes.Set{}), t.Is(tftypes.Tuple{}):
		var l []tftypes.Value
		if err := v.As(&l); err != nil {
			return nil, err
		}
		out := make([]any, 0, len(l))
		for _, e := range l {
			j, err := tfToJSON(e)
			if err != nil {
				return nil, err
			}
			out = append(out, j)
		}
		return out, nil
	}
	return nil, fmt.Errorf("unsupported type %s", t)
}

// jsonToDynamic converts a JSON value from the API to a dynamic value: objects
// become objects and arrays tuples.
func jsonToDynamic(ctx context.Context, x any) (types.Dynamic, error) {
	tf, err := jsonToTF(x)
	if err != nil {
		return types.DynamicNull(), err
	}
	v, err := basetypes.DynamicType{}.ValueFromTerraform(ctx, tf)
	if err != nil {
		return types.DynamicNull(), err
	}
	d, ok := v.(basetypes.DynamicValue)
	if !ok {
		return types.DynamicNull(), fmt.Errorf("unexpected value %T", v)
	}
	return d, nil
}

func jsonToTF(x any) (tftypes.Value, error) {
	switch x := x.(type) {
	case nil:
		return tftypes.NewValue(tftypes.DynamicPseudoType, nil), nil
	case string:
		return tftypes.NewValue(tftypes.String, x), nil
	case bool:
		return tftypes.NewValue(tftypes.Bool, x), nil
	case json.Number:
		n, ok := new(big.Float).SetString(x.String())
		if !ok {
			return tftypes.Value{}, fmt.Errorf("invalid number %s", x)
		}
		return tftypes.NewValue(tftypes.Number, n), nil
	case float64:
		return tftypes.NewValue(tftypes.Number, big.NewFloat(x)), nil
	case map[string]any:
		attrs := make(map[string]tftypes.Type, len(x))
		vals := make(map[string]tftypes.Value, len(x))
		for k, e := range x {
			v, err := jsonToTF(e)
			if err != nil {
				return tftypes.Value{}, err
			}
			attrs[k], vals[k] = v.Type(), v
		}
		return tftypes.NewValue(tftypes.Object{AttributeTypes: attrs}, vals), nil
	case []any:
		elems := make([]tftypes.Type, 0, len(x))
		vals := make([]tftypes.Value, 0, len(x))
		for _, e := range x {
			v, err := jsonToTF(e)
			if err != nil {
				return tftypes.Value{}, err
			}
			elems, vals = append(elems, v.Type()), append(vals, v)
		}
		return tftypes.NewValue(tftypes.Tuple{ElementTypes: elems}, vals), nil
	}
	return tftypes.Value{}, fmt.Errorf("unsupported JSON value %T", x)
}

// sameJSON reports whether two JSON values are equal, comparing numbers by value.
func sameJSON(a, b any) bool {
	return reflect.DeepEqual(normalizeJSON(a), normalizeJSON(b))
}

func normalizeJSON(x any) any {
	switch x := x.(type) {
	case json.Number:
		f, _ := new(big.Float).SetString(x.String())
		if f == nil {
			return x.String()
		}
		return f.Text('g', -1)
	case float64:
		return big.NewFloat(x).Text('g', -1)
	case map[string]any:
		out := make(map[string]any, len(x))
		for k, v := range x {
			out[k] = normalizeJSON(v)
		}
		return out
	case []any:
		out := make([]any, len(x))
		for i, v := range x {
			out[i] = normalizeJSON(v)
		}
		return out
	}
	return x
}
