import { contextBridge } from "electron";
import { apiConfigApi } from "./modules/apiConfigApi";
import { configApi } from "./modules/configApi";
import { conversationApi } from "./modules/conversationApi";
import { workspaceApi } from "./modules/workspaceApi";
import { sshApi } from "./modules/sshApi";
import { gitApi } from "./modules/gitApi";
import { systemApi, ptyApi, windowApi } from "./modules/systemApi";
import { memoApi } from "./modules/memoApi";
import { scheduledTaskApi } from "./modules/scheduledTaskApi";
import { personalizationApi } from "./modules/personalizationApi";
import { codexApi } from "./modules/codexApi";
import { importConfigApi } from "./modules/importConfigApi";
import { pluginsApi } from "./modules/pluginsApi";
import { imageLibraryApi } from "./modules/imageLibraryApi";
import { storageApi } from "./modules/storageApi";
import { ideApi } from "./modules/ideApi";
import { petApi } from "./modules/petApi";
import { dataManagementApi } from "./modules/dataManagementApi";

export type * from "./types";

const api = {
  ...apiConfigApi,
  ...configApi,
  ...conversationApi,
  ...workspaceApi,
  ...sshApi,
  ...gitApi,
  ...systemApi,
  ...ptyApi,
  ...windowApi,
  ...memoApi,
  ...scheduledTaskApi,
  ...personalizationApi,
  ...codexApi,
  ...importConfigApi,
  ...pluginsApi,
  ...imageLibraryApi,
  ...storageApi,
  ...ideApi,
  ...petApi,
  ...dataManagementApi,
};

contextBridge.exposeInMainWorld("snow", api);

export type SnowApi = typeof api;
