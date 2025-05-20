package mymodule

import (
)
//k8sAppsV1 "k8s.io/apis_apps_v1"
#DeploymentTemplate: #ResourceTemplate & {
	name:                            string
	IMAGE_COMMAND=#containerCommand: string
	TEMPLATE_PARAMS=#parameters: [string]: #ParameterDetails
	PORTS=#ports: _

	#manifest: //k8sAppsV1.#SchemaMap."io.k8s.api.apps.v1.Deployment" &
	{
		apiVersion: "apps/v1"
		kind:       "Deployment"
		metadata: {
			generateName: name
			labels: app: name
		}
		spec: {
			replicas: "{{inputs.parameters.replicas}}"
			selector: matchLabels: app: name
			template: {
				metadata: labels: app: name
				spec: containers: #Container & {
					#parameters:       TEMPLATE_PARAMS
					#containerCommand: IMAGE_COMMAND
					#ports:            PORTS
				}
			}
		}
	}
}
