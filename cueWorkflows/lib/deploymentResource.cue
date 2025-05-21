package mymodule

import (
	k8sAppsV1 "k8s.io/apis_apps_v1"
)

#DeploymentTemplate: #ResourceTemplate & {
	name:                            string
	IMAGE_COMMAND=#containerCommand: string
	TEMPLATE_PARAMS=#parameters: [string]: #ParameterDetails
	PORTS=#ports: _

	#manifest: (#ManifestUnifier & {in: k8sAppsV1.#SchemaMap."io.k8s.api.apps.v1.Deployment"}).out &
		{
			apiVersion: "apps/v1"
			kind:       "Deployment"
			metadata: {
				generateName: name
				labels: app: name
			}
			spec: {
				replicas: (#ForInputParameter & {name: "replicas", params: #parameters, default: int}).out
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
