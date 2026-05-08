import { useEffect } from 'react';
import * as Notifications from 'expo-notifications';
import Constants from 'expo-constants';
import { Platform } from 'react-native';
import { registerPushToken } from './api';
import { useAppConfig } from './config-store';

Notifications.setNotificationHandler({
  handleNotification: async () => ({
    shouldShowAlert: true,
    shouldPlaySound: true,
    shouldSetBadge: true,
  }),
});

export async function requestAndRegisterPushToken(
  baseUrl: string,
  token: string,
  label?: string,
): Promise<string> {
  if (Platform.OS === 'android') {
    await Notifications.setNotificationChannelAsync('default', {
      name: 'default',
      importance: Notifications.AndroidImportance.MAX,
      vibrationPattern: [0, 250, 250, 250],
    });
  }

  const { status: existingStatus } = await Notifications.getPermissionsAsync();
  let finalStatus = existingStatus;
  if (existingStatus !== 'granted') {
    const { status } = await Notifications.requestPermissionsAsync();
    finalStatus = status;
  }

  if (finalStatus !== 'granted') {
    throw new Error('Push notification permission not granted');
  }

  const projectId =
    Constants.expoConfig?.extra?.eas?.projectId ?? Constants.easConfig?.projectId ?? '';

  const tokenResponse = await Notifications.getExpoPushTokenAsync(
    projectId ? { projectId } : undefined,
  );
  const pushToken = tokenResponse.data;

  await registerPushToken(baseUrl, token, pushToken, label);
  return pushToken;
}

export function usePushRegistration(): void {
  const { baseUrl, token } = useAppConfig();

  useEffect(() => {
    if (!token) return;
    // Set up notification listeners
    const sub = Notifications.addNotificationReceivedListener((_notification) => {
      // Notifications handled by the handler above
    });
    return () => sub.remove();
  }, [token, baseUrl]);
}
