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

#SNAPSHOT_MIGRATION_CONFIG: {
	indices: [...string]
	migrations: [...{
		metadata: {
			mappings: {
				properties: {...}
			},
			//
			documentBackfillConfigs: {
				indices: [...string],
				config: {
					batchSize: int,
					initialReplicas: int
				}
			}
		}
	}]
}

#REPLAYER_CONFIG: {
	speedupFactor: float
	initialReplicas: int
}

#SOURCE_MIGRATION_CONFIG: close({
  sessionName: string,
  source: #CLUSTER_CONFIG,
  snapshotAndMigrationConfigs: [...#SNAPSHOT_MIGRATION_CONFIG],
  replayerConfig: #REPLAYER_CONFIG
})

