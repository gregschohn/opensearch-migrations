package org.opensearch.migrations.replay.datahandlers.http;

import io.netty.handler.codec.Headers;

import java.io.InputStream;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Supplier;

public class HttpMessageAsJson extends LinkedHashMap<String, Object> {
    public final static String METHOD = "method";
    public final static String URI = "URI";
    public final static String PROTOCOL = "protocol";
    public final static String HEADERS = "headers";
    public final static String PAYLOAD = "payload";


    public String method() {
        return (String) this.get(METHOD);
    }
    public void setMethod(String value) {
        this.put(METHOD, value);
    }
    public String uri() {
        return (String) this.get(URI);
    }
    public void setUri(String value) {
        this.put(URI, value);
    }

    public String protocol() {
        return (String) this.get(PROTOCOL);
    }
    public void setProtocol(String value) {
        this.put(PROTOCOL, value);
    }

    public Map<String, List<String>> headers() {
        return (Map<String, List<String>>) this.get(HEADERS);
    }
    public void setHeaders(Map<String, List<String>> value) {
        this.put(HEADERS, value);
    }
    public LazyLoadingPayloadMap payload() {
        return (LazyLoadingPayloadMap) this.get(PAYLOAD);
    }
    public void setPayload(LazyLoadingPayloadMap value) {
        this.put(PAYLOAD, value);
    }
}
