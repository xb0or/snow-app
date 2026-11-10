import type { ApiConfigInput } from "../native/types";
import type { SnowCliProfile } from "../snowCli/profiles";
import { isRecord, toBoolean, toIntegerOrNull, toText } from "../utils/value";
import { resolveAutoCompressThreshold } from "./apiConfigThreshold";

export const toApiConfigInput = (profile: SnowCliProfile): ApiConfigInput => {
  const snowcfg = isRecord(profile.config.snowcfg)
    ? profile.config.snowcfg
    : {};
  const maxContextTokens = toIntegerOrNull(snowcfg.maxContextTokens);
  const autoCompressThreshold = resolveAutoCompressThreshold(
    snowcfg.autoCompressThreshold,
    snowcfg.autoCompressThresholdPercent,
    maxContextTokens
  );

  return {
    profileName: profile.name,
    displayName: profile.name,
    isActive: profile.isActive,
    baseUrl: toText(snowcfg.baseUrl, "https://api.openai.com/v1"),
    baseUrlMode: toText(snowcfg.baseUrlMode, "auto"),
    apiKey: toText(snowcfg.apiKey),
    requestMethod: toText(snowcfg.requestMethod, "chat"),
    advancedModel: toText(snowcfg.advancedModel),
    basicModel: toText(snowcfg.basicModel),
    supportsVision: toBoolean(snowcfg.supportsVision, true),
    visionBaseUrl: toText(snowcfg.visionBaseUrl),
    visionBaseUrlMode: toText(snowcfg.visionBaseUrlMode, "auto"),
    visionApiKey: toText(snowcfg.visionApiKey),
    visionRequestMethod: toText(snowcfg.visionRequestMethod, "chat"),
    visionModel: toText(snowcfg.visionModel),
    maxContextTokens: maxContextTokens ?? undefined,
    maxTokens: toIntegerOrNull(snowcfg.maxTokens) ?? undefined,
    streamIdleTimeoutSec:
      toIntegerOrNull(snowcfg.streamIdleTimeoutSec) ?? undefined,
    enableAutoCompress: toBoolean(snowcfg.enableAutoCompress, true),
    autoCompressThreshold: autoCompressThreshold ?? undefined,
    contextKernelEnabled: toBoolean(snowcfg.contextKernelEnabled, false),
    maxRetries: toIntegerOrNull(snowcfg.maxRetries) ?? undefined,
    retryBaseDelayMs: toIntegerOrNull(snowcfg.retryDelayMs) ?? undefined,
    partialRetryMaxChars: toIntegerOrNull(snowcfg.partialRetryMaxChars) ?? undefined,
    systemPromptIdsJson: toText(snowcfg.systemPromptIdsJson),
    customHeaderSchemeId: toText(snowcfg.customHeaderSchemeId),
    configJson: JSON.stringify(profile.config),
    source: "snow-cli",
  };
};

export const normalizeApiConfigInput = (value: unknown): ApiConfigInput => {
  if (!isRecord(value)) {
    throw new Error("API config payload must be an object");
  }

  const profileName = toText(value.profileName).trim();

  if (!profileName) {
    throw new Error("Profile name is required");
  }

  const displayName =
    toText(value.displayName, profileName).trim() || profileName;
  const baseUrl =
    toText(value.baseUrl, "https://api.openai.com/v1").trim() ||
    "https://api.openai.com/v1";
  const requestMethod = toText(value.requestMethod, "chat").trim() || "chat";
  const advancedModel = toText(value.advancedModel).trim();
  const basicModel = toText(value.basicModel).trim();
  const isActive = toBoolean(value.isActive, false);

  if (isActive && !advancedModel) {
    throw new Error("Advanced model is required for an active API profile");
  }
  if (isActive && !basicModel) {
    throw new Error("Basic model is required for an active API profile");
  }

  const supportsVision = toBoolean(value.supportsVision, true);
  const visionBaseUrl = toText(value.visionBaseUrl).trim();
  const visionModel = toText(value.visionModel).trim();
  const source = toText(value.source, "manual").trim() || "manual";
  const visionRequestMethod =
    toText(value.visionRequestMethod, requestMethod).trim() || requestMethod;
  const manualConfig = {
    snowcfg: {
      baseUrl,
      baseUrlMode: toText(value.baseUrlMode, "custom"),
      requestMethod,
      advancedModel,
      basicModel,
      supportsVision,
      visionBaseUrl,
      visionBaseUrlMode: toText(value.visionBaseUrlMode, "auto"),
      visionRequestMethod,
      visionModel,
      maxContextTokens: toIntegerOrNull(value.maxContextTokens) ?? undefined,
      maxTokens: toIntegerOrNull(value.maxTokens) ?? undefined,
      streamIdleTimeoutSec:
        toIntegerOrNull(value.streamIdleTimeoutSec) ?? undefined,
      enableAutoCompress: toBoolean(value.enableAutoCompress, true),
      autoCompressThreshold:
        toIntegerOrNull(value.autoCompressThreshold) ?? undefined,
      contextKernelEnabled: toBoolean(value.contextKernelEnabled, false),
      source,
    },
  };

  return {
    profileName,
    previousProfileName: toText(value.previousProfileName).trim() || undefined,
    displayName,
    isActive,
    baseUrl,
    baseUrlMode: toText(value.baseUrlMode, "custom"),
    apiKey: toText(value.apiKey),
    requestMethod,
    advancedModel,
    basicModel,
    supportsVision,
    visionBaseUrl,
    visionBaseUrlMode: toText(value.visionBaseUrlMode, "auto"),
    visionApiKey: toText(value.visionApiKey),
    visionRequestMethod,
    visionModel,
    maxContextTokens: toIntegerOrNull(value.maxContextTokens) ?? undefined,
    maxTokens: toIntegerOrNull(value.maxTokens) ?? undefined,
    streamIdleTimeoutSec:
      toIntegerOrNull(value.streamIdleTimeoutSec) ?? undefined,
    enableAutoCompress: toBoolean(value.enableAutoCompress, true),
    autoCompressThreshold:
      toIntegerOrNull(value.autoCompressThreshold) ?? undefined,
    contextKernelEnabled: toBoolean(value.contextKernelEnabled, false),
    maxRetries: toIntegerOrNull(value.maxRetries) ?? undefined,
    retryBaseDelayMs: toIntegerOrNull(value.retryBaseDelayMs) ?? undefined,
    partialRetryMaxChars: toIntegerOrNull(value.partialRetryMaxChars) ?? undefined,
    systemPromptIdsJson: toText(value.systemPromptIdsJson),
    customHeaderSchemeId: toText(value.customHeaderSchemeId),
    configJson: toText(value.configJson, JSON.stringify(manualConfig)),
    source,
  };
};
