import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getPushTokens, tickIdleScheduler } from '../api';
import { useAppConfig } from '../config-store';
import { requestAndRegisterPushToken } from '../push';
import { Platform } from 'react-native';

export function usePushTokens() {
  const { baseUrl, token, email } = useAppConfig();
  return useQuery({
    queryKey: ['push-tokens'],
    queryFn: () => getPushTokens(baseUrl, token),
    enabled: !!(token || email),
  });
}

export function useRegisterPushToken() {
  const { baseUrl, token } = useAppConfig();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => {
      const label = `${Platform.OS}-${Platform.Version}`;
      return requestAndRegisterPushToken(baseUrl, token, label);
    },
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['push-tokens'] });
    },
  });
}

export function useTickIdleScheduler() {
  const { baseUrl, token } = useAppConfig();
  return useMutation({
    mutationFn: () => tickIdleScheduler(baseUrl, token),
  });
}
