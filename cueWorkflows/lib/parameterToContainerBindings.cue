package mymodule

import "strings"

#ParameterAndEnvironmentName: {
	parameterName: string
	envName:       string | *strings.ToUpper(parameterName)
	...
}

#FullyProjectedTemplateParameter: #ParameterDetails & #ParameterAndEnvironmentName & #ParameterAndInputPath

#Container: {
	#parameters: [string]: #ParameterDetails
	#containerCommand: string
	#ports: {...}
	_projected: [for p, details in #parameters {details & #FullyProjectedTemplateParameter & {parameterName: p}}]

	_args: strings.Join([
		for i in _projected {
			if i.type == "bool" {
				"""
                if [ "$\(i.envName)" = "true" ] || [ "$\(i.envName)" = "1" ]; then
                    ARGS="${ARGS} --(i.parameterName)"
                fi
                """
			} // for else support, see https://github.com/cue-lang/cue/issues/2122
			if i.type != "bool" {
				"""
                ARGS=\"${ARGS}${\(i.envName):+ --\(i.parameterName) $\(i.envName)}
                """
			}
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

	env: [for p in _projected {name: p.envName, value: p.templateInputPath}]
	command: [
		"/bin/sh",
		"-c",
		_commandText,
	]
	if len(#ports) != 0 {
		ports: #ports
	}
}
