package mymodule

//import "strconv"
import "strings"

#Expr: {
  ExpType: {
	  type: bool | string | number | [...] | {...} // Similar to #LiteralValue -
	  if (#IsConcreteValue & {#value: type}).concrete {
		  error("Type value for an expression should never be a concrete value")
  	}
  }

  EvaluateList: {
  	list!: [...#ArgoExpressionValue]
  	out: [ for v in list { (#ArgoExpressionValueProperties & { #in: v }).value } ]
  }

 	Concat: //#Expression &
 	{
 		op: "concat"
 		list!: [...#ArgoExpressionValue]
		type: string
	}

	Evaluate: {
		#e: _
 	  out: [
 	  	if (#e.op == Concat.op) {
 	  		strings.Join((EvaluateList & { list: #e.list }).out, "")
 	  	},
 	  	// "INVALID_EXPRESSION! \(#e.op)"
 		 ][0]
 	}
}

//#Expression: {...,
//	op: "concat" | "jsonPath"
//  type: _
//}//#Expr.UnaryExpression | #Expr.BinaryExpression | #Expr.ListExpression | #Expr.StructExpression
