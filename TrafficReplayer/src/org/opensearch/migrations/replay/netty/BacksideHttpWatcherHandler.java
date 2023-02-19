package org.opensearch.migrations.replay.netty;

import io.netty.channel.Channel;
import io.netty.channel.ChannelHandlerContext;
import io.netty.channel.SimpleChannelInboundHandler;
import io.netty.handler.codec.http.FullHttpRequest;
import org.opensearch.migrations.replay.AggregatedRawResponse;

import java.util.function.Consumer;

public class BacksideHttpWatcherHandler extends SimpleChannelInboundHandler<FullHttpRequest> {

    private final AggregatedRawResponse.Builder aggregatedRawResponseBuilder;
    private boolean doneReadingRequest; // later, when connections are reused, switch this to a counter?
    Consumer<AggregatedRawResponse> responseCallback;

    public BacksideHttpWatcherHandler(AggregatedRawResponse.Builder aggregatedRawResponseBuilder) {

        this.aggregatedRawResponseBuilder = aggregatedRawResponseBuilder;
        doneReadingRequest = false;
    }

    @Override
    protected void channelRead0(ChannelHandlerContext ctx, FullHttpRequest msg) throws Exception {
        doneReadingRequest = true;
        if (this.responseCallback != null) {
            this.responseCallback.accept(aggregatedRawResponseBuilder.build());
        }
        super.channelReadComplete(ctx);
    }

    public void addCallback(Consumer<AggregatedRawResponse> callback) {
        if (doneReadingRequest) {
            callback.accept(aggregatedRawResponseBuilder.build());
        } else {
            this.responseCallback = callback;
        }
    }

    @Override
    public void channelInactive(ChannelHandlerContext ctx) throws Exception {
        System.err.println("inactive channel - closing");
        super.channelInactive(ctx);
    }

}