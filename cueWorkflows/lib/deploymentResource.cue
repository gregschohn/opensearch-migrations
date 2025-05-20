package mymodule

#DeploymentTemplate: #ResourceTemplate & {
	  name: string
    IMAGE_COMMAND=#containerCommand: string
    TEMPLATE_PARAMS=#parameters: [string]: #ParameterDetails
    PORTS=#ports: _

		#manifest: {
			apiVersion: "apps/v1",
			kind: "Deployment",
			metadata: {
				generateName: name
				labels: {
					app: name
				}
			},
			spec: {
				replicas: "{{inputs.parameters.replicas}}",
				selector: {
					matchLabels: {
						app: name
					}
				},
				template: {
					metadata: {
						labels: {
							app: name
						}
					}
					spec: {
						containers: #Container & {
								#parameters: TEMPLATE_PARAMS
								#containerCommand: IMAGE_COMMAND
								#ports: PORTS
						}
					}
        }
			}
    }
}
