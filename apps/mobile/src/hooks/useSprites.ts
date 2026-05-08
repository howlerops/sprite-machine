import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getSprites, getSprite, suspendSprite, resumeSprite } from '../api';
import { useAppConfig } from '../config-store';

export function useSprites() {
  const { baseUrl, token, email } = useAppConfig();
  return useQuery({
    queryKey: ['sprites'],
    queryFn: () => getSprites(baseUrl, token),
    enabled: !!(token || email),
  });
}

export function useSprite(id: string) {
  const { baseUrl, token, email } = useAppConfig();
  return useQuery({
    queryKey: ['sprite', id],
    queryFn: () => getSprite(baseUrl, token, id),
    enabled: !!(token || email) && !!id,
  });
}

export function useSuspendSprite(id: string) {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => suspendSprite(baseUrl, token, id),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['sprite', id] });
      void queryClient.invalidateQueries({ queryKey: ['sprites'] });
    },
  });
}

export function useResumeSprite(id: string) {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => resumeSprite(baseUrl, token, id),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['sprite', id] });
      void queryClient.invalidateQueries({ queryKey: ['sprites'] });
    },
  });
}
