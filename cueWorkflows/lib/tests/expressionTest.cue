import "strings"

Test: {
	#Alt: {


#Value: string | #Expression

#Expression: {
    concat: [...#Value]
}

#Eval: {


    input: #Value
    result: string

    // Recursive evaluation
    result: [
    	if ((input & string) != _|_) {
    		input
    	},
	    if ((input & #Expression) != _|_) {
  	  	strings.Join(
  	  		[for v in input.concat {
  	  			(#Eval & {input: v}).result
	      	 }],
  	     	"")
    	}
    ][0]
}


example1: #Eval & {
    input: "hello"
}

example2: #Eval & {
    input: {
        concat: ["hello", " ", "world"]
    }
}

example3: #Eval & {
    input: {
        concat: [
            "start: ",
            {
                concat: ["a", "b"]
            },
            " end"
        ]
    }
}

	}
	Expressions: {
		let EMPTY_CONCAT_FROM_EXPR = { expression: #Expr.Concat & { list: [] } }
		testParam: (#ParameterWithName & {
			parameterName: "testStr", parameterDefinition: #TemplateParameterDefinition & { type: string }
		})
		GetExpressionProperties: {
			obj: (#ArgoExpressionValueProperties & { #in: { paramWithName: testParam } }).value
		}
		EvaluateList: {
			SingleString: {
				obj: (#Expr.EvaluateList & { list: ["hi"]}).out
			}
			SingleInt: {
				obj: (#Expr.EvaluateList & { list: [ 7 ]}).out
			}
			MixedLiterals: {
				obj: (#Expr.EvaluateList & { list: [ "hi", 7 ]}).out
			}
			SingleParam: {
				obj: (#Expr.EvaluateList & { list: [ { paramWithName: testParam }]}).out
			}
			SingleArgoReady: {
				obj: (#Expr.EvaluateList & { list: [ { argoReadyString: "nothing fancy" }]}).out
			}
			Mixed: {
				obj: (#Expr.EvaluateList & { list: [
					{ paramWithName: testParam },
					" -- ",
					{ argoReadyString: "nothing fancy" },
					": ",
					99
				]}).out
				_assertUnify: obj & ["{{inputs.parameters['testStr']}}", " -- ", "nothing fancy", ": ", "99"]
			}
			Expr: {
				obj: (#Expr.EvaluateList & { list: [ EMPTY_CONCAT_FROM_EXPR ]}).out
			}
		  Nested: {
		  	t1: (#ArgoExpressionValueProperties & { #in: expression: #Expr.Concat & { list: ["hi"] } }).value
		  	t2: (#ArgoExpressionValueProperties & { #in: { argoReadyString: "ready" } }).value
			}
		  NestedMore: {
		  	t1: (#ArgoExpressionValueProperties & { #in: expression: {
			  	#Expr.Concat & { list: [
			  		{ argoReadyString: "ready" },
			  		{ expression: #Expr.Concat & { list: ["hi"] } }
			  	]}
			  }}).value
			}
		}
  	Concat: {
	  	Empty: {
		  	obj: (#ArgoExpressionValueProperties & { #in: { expression: #Expr.Concat & { list: [] } } } ).value
			  _assert: obj & ""
  		}
	  	Simple: {
	  		obj: (#ArgoExpressionValueProperties & { #in: { expression: #Expr.Concat & { list: [ "Hello"] } } } ).value
	  		_assert: obj & "Hello"
			}
		  Mixed: {
				_e: (#Expr.Concat & { list: [
					{ paramWithName: testParam },
					" -- ",
					{ argoReadyString: "nothing fancy" },
					": ",
					99
				]})
				obj: (#ArgoExpressionValueProperties & { #in: { expression: _e } } ).value
				_assertUnify: obj & "{{inputs.parameters['testStr']}} -- nothing fancy: 99"
			}
		  NestedMore: {
		  	//fromP: ""//{ argoReadyString: "nothing fancy" }//{ paramWithName: #ParameterWithName & { parameterDefinition: #TemplateParameterDefinition, parameterName: "testStr"} }
		  	fromP: "NO"// expression: #Expr.Concat & { list: [ "HI" ] }
				e: (#Expr.Concat & { list: [
					{ expression: #Expr.Concat & { list: [ fromP ] } },
					" -- ",
					{ argoReadyString: "nothing fancy" },
					": ",
					99
				]})
		  	obj: (#ArgoExpressionValueProperties & { #in: expression: {
		  		#Expr.Concat & { list: [
			  		{ expression: #Expr.Concat & { list: [
//			  				{ expression: #Expr.Concat & { list: ["Hi,", " ", "world!" ] } },
                "w","ow"
			  		 ] } },
			  		{ argoReadyString: "ready" },
			  		{ expression: #Expr.Concat & { list: ["hi"] } }
			  	]}
		  	} } ).value
//				_assertUnify: obj & "{{inputs.parameters['testStr']}} -- nothing fancy: 99"
			}
		}
	}
}
