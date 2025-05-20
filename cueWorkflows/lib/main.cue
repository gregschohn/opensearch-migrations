package mymodule

// Argo will do this substitution, not CUE - but useful for documenting what the structure will be
#captureContext: close({
    sessionName: "dummy",
    sourceConfig: #CLUSTER_CONFIG & {
        name: "firstSource"
        endpoint: "http://elasticsearch-master:9200"
        allow_insecure: true
        authConfig: {
            region: "us-east-2"
        }
    }
})


#HTTP_AUTH_BASIC: close({
    username!: string
    password!: string
})

#HTTP_AUTH_SIGV4: close({
    region!: string
    service?: string
})

#CLUSTER_CONFIG: close({
    name: string
    endpoint: string
    allow_insecure?: bool
    version?: string
    authConfig?: #HTTP_AUTH_BASIC | #HTTP_AUTH_SIGV4
})
