 {
  apiVersion: "v1",
  kind: "List",
  items: [
    #MIGRATION_TEMPLATES.CAPTURE_REPLAY,
    #MIGRATION_TEMPLATES.FULL,
    #MIGRATION_TEMPLATES.PROXY,
		#MIGRATION_TEMPLATES.REPLAYER,
		#MIGRATION_TEMPLATES.TARGET_LATCH_HELPERS
	]
}

//w: (#InlineInputParameter & {name: "etcdImage", params: #WORKFLOW_PARAMS}).out,
//t: (#InlineInputParameter & {name: "repo_uri", params: #S3_PARAMS}).out,