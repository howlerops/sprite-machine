import { useState } from 'react';
import { Platform, Linking } from 'react-native';
import { useRouter } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Heading } from '@/components/ui/heading';
import { Button, ButtonText, ButtonSpinner } from '@/components/ui/button';
import { Input, InputField, InputSlot } from '@/components/ui/input';
import { Icon, EyeIcon, EyeOffIcon } from '@/components/ui/icon';
import {
  FormControl,
  FormControlError,
  FormControlErrorText,
  FormControlHelper,
  FormControlHelperText,
} from '@/components/ui/form-control';
import { Link, LinkText } from '@/components/ui/link';

import { useAppConfig } from '../src/config-store';
import { setEmail } from '../src/storage';
import { login } from '../src/api';

type LoginError = 'invalid_token' | 'not_allowed' | 'network' | null;

function errorMessage(e: LoginError): string {
  switch (e) {
    case 'invalid_token':
      return "That token didn't work. Check `flyctl auth token` and retry.";
    case 'not_allowed':
      return "Your Fly account isn't on the allowlist. Contact the operator.";
    case 'network':
      return "Couldn't reach the control plane. Check your connection.";
    default:
      return '';
  }
}

export default function LoginScreen() {
  const router = useRouter();
  const { baseUrl, setEmail: setEmailCtx } = useAppConfig();
  const [flyToken, setFlyToken] = useState('');
  const [showToken, setShowToken] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<LoginError>(null);

  async function handleContinue() {
    const trimmed = flyToken.trim();
    if (!trimmed) return;
    setPending(true);
    setError(null);
    try {
      const result = await login(baseUrl, trimmed);
      await setEmail(result.email);
      setEmailCtx(result.email);
      router.replace('/');
    } catch (err: unknown) {
      const status = (err as { status?: number })?.status;
      if (!status) {
        setError('network');
      } else if (status === 401) {
        setError('invalid_token');
      } else if (status === 403) {
        setError('not_allowed');
      } else {
        setError('network');
      }
    } finally {
      setPending(false);
    }
  }

  // On mobile (base): full-bleed, no card border
  const cardClass =
    Platform.OS === 'web'
      ? 'w-full max-w-md bg-card border border-border rounded-[10px] p-6 md:p-8'
      : 'w-full bg-transparent p-4';

  const hasError = error !== null;

  return (
    <Box className="flex-1 bg-background items-center justify-center px-4 md:px-8">
      <VStack space="xl" className={cardClass}>
        {/* Brand */}
        <HStack space="sm" className="items-center">
          <Heading size="lg" bold className="text-foreground">
            sprite-machine
          </Heading>
          <Box className="w-2 h-2 rounded-full bg-status-running" />
        </HStack>

        <VStack space="xs">
          <Heading size="xl" className="text-foreground">
            Sign in
          </Heading>
          <Text size="sm" className="text-muted-foreground">
            Paste your Fly personal access token. Run{' '}
            <Text size="sm" className="font-mono text-foreground">
              flyctl auth token
            </Text>{' '}
            in your terminal, or generate one at fly.io. We exchange it for a session and discard
            the token.
          </Text>
        </VStack>

        <FormControl isInvalid={hasError}>
          <Input>
            <InputField
              value={flyToken}
              onChangeText={setFlyToken}
              secureTextEntry={!showToken}
              autoCapitalize="none"
              autoCorrect={false}
              placeholder="fo1_xxx..."
            />
            <InputSlot onPress={() => setShowToken((v) => !v)} className="pr-3">
              <Icon
                as={showToken ? EyeOffIcon : EyeIcon}
                size="sm"
                className="text-muted-foreground"
              />
            </InputSlot>
          </Input>
          {hasError ? (
            <FormControlError>
              <FormControlErrorText>{errorMessage(error)}</FormControlErrorText>
            </FormControlError>
          ) : (
            <FormControlHelper>
              <FormControlHelperText>Your token is never stored server-side.</FormControlHelperText>
            </FormControlHelper>
          )}
        </FormControl>

        <Button
          size="lg"
          variant="default"
          className="w-full"
          isDisabled={pending || !flyToken.trim()}
          onPress={() => void handleContinue()}
        >
          {pending ? <ButtonSpinner /> : null}
          <ButtonText>Continue</ButtonText>
        </Button>

        <HStack space="xs" className="justify-center items-center">
          <Text size="xs" className="text-muted-foreground">
            Need a token?
          </Text>
          <Link
            onPress={() => void Linking.openURL('https://fly.io/dashboard/account/access-tokens')}
          >
            <LinkText size="xs">Get one from fly.io</LinkText>
          </Link>
        </HStack>
      </VStack>
    </Box>
  );
}
