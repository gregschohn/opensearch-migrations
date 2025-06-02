package mymodule

#HTTP_AUTH_BASIC: close({
  username!: string
  password!: string
})

#HTTP_AUTH_SIGV4: close({
  region!:  string
  service?: string
})

#CLUSTER_CONFIG: close({
  name:            string
  endpoint:        string
  allow_insecure?: bool
  version?:        string
  authConfig?:     #HTTP_AUTH_BASIC | #HTTP_AUTH_SIGV4
})

#SOURCE_MIGRATION_CONFIG: close({
  sessionName: "dummy"
  source: #CLUSTER_CONFIG & {
    name:           "firstSource"
    endpoint:       "http://elasticsearch-master:9200"
    allow_insecure: true
    authConfig: region: "us-east-2"
  },
  snapshotAndMigrationConfigs: [
  	{
  		indices: [...string]
			migrations: [...{
				metadata: {
					mappings: {
						properties: {...}
					},
					documentBackfillConfigs: {
						indices: [...string],
						config: {
							batchSize: int,
							initialReplicas: int
						}
					},
				}
			}]
		}
  ],
  replayerConfig: {
  	speedupFactor: float
  	initialReplicas: int
  }
})

