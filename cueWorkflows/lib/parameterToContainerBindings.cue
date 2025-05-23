package mymodule

import "strings"

import k8sAppsV1 "k8s.io/apis_apps_v1"

#ParameterAndEnvironmentName: {
  parameterName: string
  envName:       string | *strings.ToUpper(parameterName)
  ...
}

#FullyProjectedTemplateParameter: #ParameterDetails & #ParameterAndEnvironmentName & #ParameterAndInputPath

#Container: {
	Base: (#ManifestUnifier & {in: k8sAppsV1.#SchemaMap."io.k8s.api.core.v1.Container"}).out & {
			#parameters: [string]: #ParameterDetails
			#ports: [...{...}]

			_filteredParameters: { for k,v in #parameters if v.passToContainer { (k): v } }
			_enrichedParameters: [for p, details in _filteredParameters { 
				details & #FullyProjectedTemplateParameter & {parameterName: p} 
			}]

			env: [for p in _enrichedParameters {name: p.envName, value: p.templateInputPath}]
			if len(#ports) != 0 {
				ports: #ports
			}
		}

	Jib: Base & {
		// should be able to use the default entrypoint
	}

  Bash: Base & {
			#containerCommand: string
			_enrichedParameters: _

			_args: strings.Join([
				for i in _enrichedParameters {[
					if (i.type == "bool") {
						"""
						if [ "$\(i.envName)" = "true" ] || [ "$\(i.envName)" = "1" ]; then
								ARGS="${ARGS} --(i.parameterName)"
						fi
						"""
					},
					if (i.type != "bool")  {
						"""
						ARGS=\"${ARGS}${\(i.envName):+ --\(i.parameterName) $\(i.envName)}
						"""
					}][0]
				}], "\n")
			_commandText:
				"""
				set -e

				# Build arguments from environment variables
				ARGS=""
				\(_args)

				# Log the configuration
				echo "Starting \(#containerCommand) with arguments: $ARGS"

				# Execute the command
				exec \(#containerCommand) $ARGS
				"""

			command: [
				"/bin/sh",
				"-c",
				_commandText,
			]
		}
}