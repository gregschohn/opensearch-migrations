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

  #ArgoExpressionValueProperties: {
  	#in: #ArgoExpressionValue

  }

  EvaluateList: {
  	#list: [...#ArgoExpressionValue]
  	out: []
  }

 	Concat: {
 		#in: [...(#ArgoParameterValue & { ..., inferredType: string })]
 		out: strings.Join((EvaluateList & { #list: #in }).out, "")
 	}

  UnaryExpression: {...}
  BinaryExpression: {...}
  ListExpression: {...}
  StructExpression: {...}
}

#Expression: {...}//#Expr.UnaryExpression | #Expr.BinaryExpression | #Expr.ListExpression | #Expr.StructExpression
