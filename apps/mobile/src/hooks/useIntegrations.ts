import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  applyGithubIntegration,
  getGithubIntegration,
  saveGithubIntegration,
  type GithubIntegration,
} from '../api';
import { useAppConfig } from '../config-store';

export function useGithubIntegration() {
  const { baseUrl, token, email } = useAppConfig();
  return useQuery<GithubIntegration>({
    queryKey: ['integration', 'github'],
    queryFn: () => getGithubIntegration(baseUrl, token),
    enabled: !!(token || email),
  });
}

export function useSaveGithubIntegration() {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: {
      token?: string;
      username: string;
      gitUserName: string;
      gitUserEmail: string;
    }) => saveGithubIntegration(baseUrl, token, body),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['integration', 'github'] });
      void queryClient.invalidateQueries({ queryKey: ['sprites'] });
    },
  });
}

export function useApplyGithubIntegration() {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => applyGithubIntegration(baseUrl, token),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['integration', 'github'] });
      void queryClient.invalidateQueries({ queryKey: ['sprites'] });
    },
  });
}
