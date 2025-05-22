package mymodule

import k8sAppsV1 "k8s.io/apis_apps_v1"

#DeploymentTemplate: #ResourceTemplate & {
	#resourceName!: string
	#parameters!: [string]: #ParameterDetails
	#containers: [{...}]

	#manifest: (#ManifestUnifier & {in: k8sAppsV1.#SchemaMap."io.k8s.api.apps.v1.Deployment"}).out &
		{
			apiVersion: "apps/v1"
			kind:       "Deployment"
			metadata: {
				generateName: "\(#resourceName)-"
				labels: app: #resourceName
			}

			spec: {
				replicas!: _
				selector: matchLabels: app: #resourceName
				template: {
					metadata: labels: app: #resourceName
					spec: containers: #containers
				}
			}
		}
}
