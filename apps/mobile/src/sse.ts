import { useEffect, useRef } from 'react';
import { Platform } from 'react-native';
import { useQueryClient } from '@tanstack/react-query';
import EventSource from 'react-native-sse';
import type { RunDetail } from './api';
import type { RunEvent } from '@sprite-machine/shared';
import { useAppConfig } from './config-store';

export function useRunEventStream(runId: string): void {
  const queryClient = useQueryClient();
  const { baseUrl, token, email } = useAppConfig();
  const lastSeqRef = useRef<number>(-1);
  const esRef = useRef<EventSource | null>(null);

  useEffect(() => {
    // Native iOS uses bearer token; web/PWA uses cookie session keyed on
    // email. We need at least one of the two to attempt a connection.
    if (!token && !email) return;

    function connect(after: number): void {
      if (esRef.current) {
        esRef.current.close();
        esRef.current = null;
      }

      const url = `${baseUrl}/runs/${runId}/events?after=${after}`;
      const es = new EventSource(
        url,
        token ? { headers: { Authorization: `Bearer ${token}` } } : { withCredentials: true },
      );
      esRef.current = es;

      es.addEventListener('message', (e) => {
        if (!e.data) return;
        try {
          const parsed = JSON.parse(e.data) as RunEvent;
          const seq = e.lastEventId ? Number(e.lastEventId) : lastSeqRef.current + 1;
          lastSeqRef.current = seq;

          queryClient.setQueryData<RunDetail>(['run', runId], (prev) => {
            if (!prev) return prev;
            const alreadyExists = prev.events.some((ev) => ev.seq === seq);
            if (alreadyExists) return prev;

            // Update status if this is a status event
            let updatedStatus = prev.status;
            if (parsed.type === 'status') {
              updatedStatus = parsed.status;
            }

            return {
              ...prev,
              status: updatedStatus,
              events: [
                ...prev.events,
                {
                  runId,
                  seq,
                  event: parsed,
                  createdAt: new Date().toISOString(),
                },
              ],
            };
          });
        } catch {
          // ignore parse errors
        }
      });

      es.addEventListener('error', () => {
        es.close();
        esRef.current = null;
        // Reconnect after 3 seconds with last known seq
        setTimeout(() => {
          connect(lastSeqRef.current);
        }, 3000);
      });
    }

    // Seed lastSeq from existing cached data
    const cached = queryClient.getQueryData<RunDetail>(['run', runId]);
    if (cached && cached.events.length > 0) {
      const maxSeq = cached.events.reduce((max, ev) => Math.max(max, ev.seq), 0);
      lastSeqRef.current = maxSeq;
    }

    // iOS Safari PWA: the session cookie sometimes isn't yet visible to the
    // first XHR opened after login. A short delay lets the browser commit
    // the cookie before SSE opens. Native bearer auth doesn't need this.
    const initialDelay = Platform.OS === 'web' && !token ? 200 : 0;
    const t = setTimeout(() => connect(lastSeqRef.current), initialDelay);

    return () => {
      clearTimeout(t);
      esRef.current?.close();
      esRef.current = null;
    };
  }, [runId, baseUrl, token, email, queryClient]);
}
