import { useEffect, useRef } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import EventSource from 'react-native-sse';
import type { RunDetail } from './api';
import type { RunEvent } from '@sprite-machine/shared';
import { useAppConfig } from './config-store';

export function useRunEventStream(runId: string): void {
  const queryClient = useQueryClient();
  const { baseUrl, token } = useAppConfig();
  const lastSeqRef = useRef<number>(0);
  const esRef = useRef<EventSource | null>(null);

  useEffect(() => {
    if (!token) return;

    function connect(after: number): void {
      if (esRef.current) {
        esRef.current.close();
        esRef.current = null;
      }

      const url = `${baseUrl}/runs/${runId}/events?after=${after}`;
      const es = new EventSource(url, {
        headers: {
          Authorization: `Bearer ${token}`,
        },
      });
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

    connect(lastSeqRef.current);

    return () => {
      esRef.current?.close();
      esRef.current = null;
    };
  }, [runId, baseUrl, token, queryClient]);
}
