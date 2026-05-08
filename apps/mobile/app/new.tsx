import { useState } from 'react';
import { useRouter } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { ScrollView } from '@/components/ui/scroll-view';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Input, InputField } from '@/components/ui/input';
import { Textarea, TextareaInput } from '@/components/ui/textarea';
import { Button, ButtonText, ButtonSpinner } from '@/components/ui/button';
import {
  FormControl,
  FormControlLabel,
  FormControlLabelText,
  FormControlError,
  FormControlErrorText,
  FormControlHelper,
  FormControlHelperText,
} from '@/components/ui/form-control';
import { useToast, Toast, ToastTitle, ToastDescription } from '@/components/ui/toast';

import { useCreateRun } from '../src/hooks/useRuns';
import type { CreateRunRequest } from '../src/api';

type PermissionMode = 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
type Harness = 'claude' | 'codex' | 'pi' | 'mock';

const PERMISSION_MODES: PermissionMode[] = ['default', 'acceptEdits', 'bypassPermissions', 'plan'];
const HARNESSES: Harness[] = ['claude', 'codex', 'pi', 'mock'];

function ChipGroup<T extends string>({
  options,
  value,
  onChange,
}: {
  options: readonly T[];
  value: T;
  onChange: (next: T) => void;
}) {
  return (
    <HStack space="sm" className="flex-wrap">
      {options.map((opt) => {
        const active = value === opt;
        return (
          <Pressable
            key={opt}
            onPress={() => onChange(opt)}
            className={
              active
                ? 'px-3 py-2 rounded-[10px] border border-primary bg-primary'
                : 'px-3 py-2 rounded-[10px] border border-border bg-background data-[hover=true]:bg-accent'
            }
          >
            <Text
              size="sm"
              bold={active}
              className={active ? 'text-primary-foreground' : 'text-foreground'}
            >
              {opt}
            </Text>
          </Pressable>
        );
      })}
    </HStack>
  );
}

export default function NewRunScreen() {
  const router = useRouter();
  const createRun = useCreateRun();
  const toast = useToast();

  const [prompt, setPrompt] = useState('');
  const [workdir, setWorkdir] = useState('');
  const [permissionMode, setPermissionMode] = useState<PermissionMode>('default');
  const [harness, setHarness] = useState<Harness>('claude');
  const [submitted, setSubmitted] = useState(false);

  const promptInvalid = submitted && !prompt.trim();

  function showError(message: string) {
    const id = String(Math.random());
    toast.show({
      id,
      placement: 'top',
      duration: 3000,
      render: () => (
        <Toast nativeID={`toast-${id}`} action="error" variant="solid">
          <ToastTitle>Error</ToastTitle>
          <ToastDescription>{message}</ToastDescription>
        </Toast>
      ),
    });
  }

  async function handleSubmit() {
    setSubmitted(true);
    const trimmed = prompt.trim();
    if (!trimmed) return;

    const body: CreateRunRequest = {
      prompt: trimmed,
      ...(harness !== 'claude' ? { harness } : {}),
      ...(workdir.trim() ? { workdir: workdir.trim() } : {}),
      ...(permissionMode !== 'default' ? { permissionMode } : {}),
    };

    try {
      const run = await createRun.mutateAsync(body);
      router.back();
      router.push(`/run/${run.id}`);
    } catch (e) {
      showError(e instanceof Error ? e.message : 'Failed to create run.');
    }
  }

  return (
    <ScrollView className="flex-1 bg-background" keyboardShouldPersistTaps="handled">
      <VStack space="lg" className="p-5 md:p-8 w-full max-w-2xl mx-auto">
        <FormControl isInvalid={promptInvalid} isRequired>
          <FormControlLabel>
            <FormControlLabelText>Prompt</FormControlLabelText>
          </FormControlLabel>
          <Textarea className="min-h-32">
            <TextareaInput
              value={prompt}
              onChangeText={setPrompt}
              autoFocus
              placeholder="What should the agent do?"
              textAlignVertical="top"
            />
          </Textarea>
          {promptInvalid ? (
            <FormControlError>
              <FormControlErrorText>Prompt is required</FormControlErrorText>
            </FormControlError>
          ) : null}
        </FormControl>

        <FormControl>
          <FormControlLabel>
            <FormControlLabelText>Harness</FormControlLabelText>
          </FormControlLabel>
          <ChipGroup options={HARNESSES} value={harness} onChange={setHarness} />
        </FormControl>

        <FormControl>
          <FormControlLabel>
            <FormControlLabelText>Working Directory</FormControlLabelText>
          </FormControlLabel>
          <Input>
            <InputField
              value={workdir}
              onChangeText={setWorkdir}
              autoCapitalize="none"
              autoCorrect={false}
              placeholder="/path/to/project"
            />
          </Input>
          <FormControlHelper>
            <FormControlHelperText>Absolute path on the sprite (optional).</FormControlHelperText>
          </FormControlHelper>
        </FormControl>

        <FormControl>
          <FormControlLabel>
            <FormControlLabelText>Permission Mode</FormControlLabelText>
          </FormControlLabel>
          <ChipGroup
            options={PERMISSION_MODES}
            value={permissionMode}
            onChange={setPermissionMode}
          />
        </FormControl>

        <Box className="mt-2">
          <Button
            variant="default"
            size="lg"
            isDisabled={createRun.isPending}
            onPress={() => void handleSubmit()}
            className="w-full"
          >
            {createRun.isPending ? <ButtonSpinner /> : null}
            <ButtonText>Start Run</ButtonText>
          </Button>
        </Box>
      </VStack>
    </ScrollView>
  );
}
