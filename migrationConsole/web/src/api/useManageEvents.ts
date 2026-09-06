import { useEffect, useState } from "react";
import type { QueryClient } from "@tanstack/react-query";

import { connectEventSource, type EventConnectionState } from "./eventSource";

export type { EventConnectionState } from "./eventSource";


export function useManageEvents(queryClient: QueryClient) {
  const [connection, setConnection] =
    useState<EventConnectionState>("connecting");

  useEffect(() => connectEventSource("/api/v1/manage/events", {
    onStateChange: setConnection,
    onRecovered: () => {
      void queryClient.invalidateQueries({ queryKey: ["manage-state"] });
    },
    listeners: {
      heartbeat: () => setConnection("live"),
      "state-invalidated": () => {
        setConnection("live");
        void queryClient.invalidateQueries({ queryKey: ["manage-state"] });
      },
    },
  }), [queryClient]);

  return connection;
}
