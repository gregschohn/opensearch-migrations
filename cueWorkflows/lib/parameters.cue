package mymodule

#ParameterDetails: {
  type!:         "bool" | "string" | "int"
  defaultValue?: string // Argo limits this to be only a string
  description?:  string
  #cueType: ([
    if (type == "int") {int},
    if (type == "bool") {bool},
    if (type == "string") {string},
  ][0])
  ...
}
