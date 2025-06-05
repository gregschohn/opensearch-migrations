package mymodule

import "list"

#IsConcreteValue: {
	#value: _
	concrete: [
		if (#value & bool) != _|_ { true },
		if (#value & bytes) != _|_ { true },
		if (#value & number) != _|_ { true },
		if (#value & string) != _|_ { true },
		// notice that we're NOT counting this as concrete. It also prevents `_` from evaluating to true.
		if (#value & null) != _|_ { false },

		if (#value & [...]) != _|_ {
			[
				//FIXME
				if ([_,...] & #value) == _|_ { true }, // empty counts as concrete
				if ([] & #value) != _|_ { false }, // able to unify to empty - but NOT empty means that we're still unbounded
				!list.Contains([for v in #value { (#IsConcreteValue & { #value: v }).concrete }], false) // if we have elements, let's check 'em
			][0]
		},

		if (#value & {...}) != _|_ {
			!list.Contains([for k, v in #value {
		      (#IsConcreteValue & {#value: v}).concrete
	        }], false)
		},

		false
	][0]
}

