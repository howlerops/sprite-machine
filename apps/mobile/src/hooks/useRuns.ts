import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getRuns, getRun, createRun, sendMessage, cancelRun, resolvePermission } from '../api';
import type { CreateRunRequest, ResolvePermissionRequest } from '../api';
import { useAppConfig } from '../config-store';

export function useRuns() {
  const { baseUrl, token } = useAppConfig();
  return useQuery({
    queryKey: ['runs'],
    queryFn: () => getRuns(baseUrl, token!),
    enabled: !!token,
    refetchInterval: 10_000,
  });
}

export function useRun(id: string) {
  const { baseUrl, token } = useAppConfig();
  return useQuery({
    queryKey: ['run', id],
    queryFn: () => getRun(baseUrl, token!, id),
    enabled: !!token,
  });
}

export function useCreateRun() {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateRunRequest) => createRun(baseUrl, token!, body),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['runs'] });
    },
  });
}

export function useSendMessage(runId: string) {
  const { baseUrl, token } = useAppConfig();
  return useMutation({
    mutationFn: (text: string) => sendMessage(baseUrl, token!, runId, text),
  });
}

export function useCancelRun(runId: string) {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => cancelRun(baseUrl, token!, runId),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['run', runId] });
      void queryClient.invalidateQueries({ queryKey: ['runs'] });
    },
  });
}

export function useResolvePermission(runId: string) {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ requestId, body }: { requestId: string; body: ResolvePermissionRequest }) =>
      resolvePermission(baseUrl, token!, runId, requestId, body),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['run', runId] });
    },
  });
}
