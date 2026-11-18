# Microsoft To Do（只读）

岛仔可以把 Microsoft To Do 里的清单和未完成任务读到待办页。这一版只读：不创建、不修改、不完成、不删除，也不把本机待办上传到 Microsoft。本机待办文件 `todo.txt` 保持原样。

这项功能不会代替 Outlook 或 Microsoft To Do。工作任务仍在原来的地方修改。岛仔只是多一个查看入口。Outlook、Hotmail、Live 和 Microsoft 365 **邮箱**的 OAuth 仍然不可用，邮件设置里的说明不变。

这一版**没有**用真实的 Microsoft 账号完成登录。下面的注册步骤对照了 2026-11-18 的 Microsoft Learn，仓库里也**没有**预置应用程序（客户端）ID。填入的 ID 必须是你自己注册的。

## 权限

只申请这些范围：

`openid profile offline_access Tasks.Read`

| 范围 | 作用 |
| --- | --- |
| `Tasks.Read` | 委托权限。读取登录用户自己的任务和清单，包括分享给该用户的清单。权限说明写明不包含创建、删除或更新。工作或学校账号、个人 Microsoft 账号都可以同意。管理同意不是这项权限本身的要求，但单位策略仍可能禁止用户自行同意。 |
| `offline_access` | 用来取得刷新令牌，访问令牌过期后不必每次都打开浏览器。 |
| `openid`、`profile` | 从 ID 令牌里读取登录名，用来显示是哪个账号。不申请 `User.Read`，也不申请任何 `Mail.*`、`Tasks.ReadWrite`、`Tasks.Read.All`。 |

对应的 Graph 调用只有 GET：

- `GET /me/todo/lists`
- `GET /me/todo/lists/{id}/tasks`，并带 `status ne 'completed'`

列表接口的最低权限在 Microsoft 文档中就是委托的 `Tasks.Read`：[List tasks](https://learn.microsoft.com/en-us/graph/api/todotasklist-list-tasks?view=graph-rest-1.0)、[权限说明](https://learn.microsoft.com/en-us/graph/permissions-reference)。`Tasks.Read` 的委托权限 ID 为 `f45671fb-e0fe-4b4b-be20-3d3ce43f1bcb`。

## 账号能连哪些

授权地址固定是全球云 `https://login.microsoftonline.com`，数据地址固定是 `https://graph.microsoft.com`。不读取由世纪互联运营的 Azure，也不读取美国政府云。List tasks 文档的国家云表格里，由世纪互联运营的 Microsoft Graph **没有**这项接口。

在设置里选账号范围：

| 设置 | 授权租户 | 应用注册要选的受支持账户类型 |
| --- | --- | --- |
| 个人和工作 | `common` | 任何组织目录中的帐户和个人 Microsoft 帐户 |
| 仅工作或学校 | `organizations` | 任何组织目录中的帐户 |
| 仅个人 | `consumers` | 仅个人 Microsoft 帐户 |
| 另填目录（租户）ID | 该 GUID | 仅此组织目录中的帐户（单租户） |

单租户应用如果仍用 `common`，Microsoft 会拒绝。目录 ID 填在应用注册概述页的「目录(租户) ID」，只接受 GUID。个人账号不能用单租户的工作目录登录。

待办页关闭后不再发起新的读取。断网或服务失败时保留上一份成功结果，并显示错误；登录失效时需要重新连接。自动读取间隔大约 5 分钟，遇到 `429` 会按 `Retry-After` 等待，并限制在 30 秒到 1 小时之间。

## 注册应用

桌面程序是公开客户端，不能安全保存客户端机密。授权码流程的文档写明：不要在本机应用里使用 client secret。[授权码流程](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-auth-code-flow)、[公开客户端与机密客户端](https://learn.microsoft.com/en-us/entra/identity-platform/msal-client-applications)。

1. 打开 [Microsoft Entra 管理中心](https://entra.microsoft.com/) → 应用注册 → 新注册。也可以从 [桌面应用注册说明](https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-app-registration) 进入。
2. 名称自定。受支持的账户类型按上一节选择，并和岛仔里的「账号范围」一致。
3. 注册完成后，从概述页复制「应用程序(客户端) ID」，粘贴到岛仔设置 → 待办 → 应用 ID。不要把别的软件的客户端 ID 或令牌拿来填。
4. 身份验证 → 添加平台 → **移动和桌面应用程序**。重定向 URI 填写 `http://localhost`。系统浏览器使用这个地址；端口在匹配时会被忽略，所以不要为每个端口各注册一条。说明见 [重定向 URI](https://learn.microsoft.com/en-us/entra/identity-platform/reply-url)。
5. 同一页的高级设置里，把「允许公共客户端流」设为是，然后保存。不要创建客户端密码或证书。
6. API 权限 → 添加权限 → Microsoft Graph → 委托的权限 → 勾选 `Tasks.Read`。不要添加 `Tasks.ReadWrite`。然后授予用户同意；如果单位禁止用户同意，请管理员只同意 `Tasks.Read`。
7. 在岛仔里点「连接」。浏览器会要求选择账号并同意读取待办。岛仔使用 PKCE（`S256`）和 `form_post`，回调只接受本机地址。同意后可以关闭浏览器页，回到岛仔。

岛仔会打开：

`https://login.microsoftonline.com/{租户}/oauth2/v2.0/authorize`

其中包含 `response_type=code`、`response_mode=form_post`、`code_challenge_method=S256`、`prompt=select_account`，以及上面列出的范围。令牌请求发到同一租户的 `/oauth2/v2.0/token`，正文里没有 `client_secret`。

## 令牌和缓存

| 数据 | 位置 |
| --- | --- |
| 访问令牌、刷新令牌 | Windows 凭据管理器，服务名 `Daozai.MicrosoftToDo`，账户名 `session` |
| 应用 ID、账号范围、目录 ID | `settings.txt`。应用 ID 不是机密 |
| 清单名、未完成任务标题、到期时间、账号名称 | 数据目录的 `ms-todo-cache.json` |

日志和错误提示不会带回令牌、授权码或响应正文。断开连接会删除凭据和 `ms-todo-cache.json`。非 Windows 系统可以编译和运行界面，但不能把登录写入凭据管理器，因此不能完成连接。

ID 令牌只用来显示账号名称，岛仔不在本地校验它的签名；访问令牌是否有效由 Microsoft Graph 判断。

## 时间和界面

没有具体时间、并且时区是 UTC 的到期日，按日期本身显示，不把 UTC 零点换算到前一天。带时间的 UTC 时刻会换算到电脑时区。`China Standard Time` 等少数 Windows 时区名会换算；认不出的时区按待办里的钟点原样显示，并在悬停里说明没有换算。

待办页「微软」里的任务不能勾选。修改仍去 Microsoft To Do 或 Outlook。岛仔也不为这些任务另弹一套到期提醒，避免和工作软件的提醒重复。最多保留 200 条未完成任务。

## 这一版没有做的事

- 没有真实租户的登录、刷新和 Graph 读取记录。连接是否成功要在你的 Windows 电脑上按下面的清单确认。
- 没有世纪互联或美国政府云的终结点。
- 没有任务正文、检查项、完成或任何写入。
- Windows 凭据管理器的往返测试只在 Windows 上编译；这次检查运行在 Linux 上，没有执行该测试，也没有生成 Windows 安装包。

## 在 Windows 上手工确认

1. 按上文注册应用，只添加委托的 `Tasks.Read`，重定向 URI 为 `http://localhost`，允许公共客户端流，不创建机密。
2. 设置里粘贴自己的应用程序 ID，账号范围与注册一致。点连接，在浏览器选择工作或个人账号并同意。
3. 确认设置里出现账号名称，待办页「微软」能看到未完成任务和到期日。已完成任务不出现。
4. 点一条 Microsoft 任务，岛上提示只读；本机待办的勾选、编辑、删除仍只影响本机。
5. 断开网络后再打开，应显示上次读取和错误，而不是空白冒充最新数据。
6. 点断开。Windows 凭据管理器里不再有 `Daozai.MicrosoftToDo` / `session`，数据目录里不再有 `ms-todo-cache.json`。
7. 确认没有 `client_secret`，诊断日志里没有访问令牌或刷新令牌。
