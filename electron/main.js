// electron/main.js
import { app, BrowserWindow, nativeImage } from 'electron'
import * as path from 'path'
import { fileURLToPath } from 'url'

const __filename = fileURLToPath(import.meta.url)
const __dirname = path.dirname(__filename)

function getIconPath() {
  // Determine if we're in dev or production
  const isDev = !!process.env.VITE_DEV_SERVER_URL;

  // Get the base path depending on dev/prod environment
  const basePath = isDev ? path.join(__dirname, '../') : path.join(__dirname, '../..');

  // Get the appropriate icon based on platform
  switch (process.platform) {
    case 'win32':
      return path.join(basePath, 'build/icons/win/icon.ico');
    case 'darwin':
      return path.join(basePath, 'build/icons/mac/icon.icns');
    default: // linux
      return path.join(basePath, 'build/icons/png/1024x1024.png');
  }
}

function createWindow() {
  // Create the browser window
  const win = new BrowserWindow({
    width: 1200,
    height: 800,
    webPreferences: {
      nodeIntegration: true,
      contextIsolation: false,
    },
    icon: getIconPath(),
    backgroundColor: '#1e293b', // Match the app's dark theme
  })

  // Set the app icon for Windows
  if (process.platform === 'win32') {
    const icon = nativeImage.createFromPath(getIconPath())
    win.setIcon(icon)
    app.setAppUserModelId('com.jsonviewer.app')
  }

  // In development, load from the Vite dev server
  if (process.env.VITE_DEV_SERVER_URL) {
    win.loadURL(process.env.VITE_DEV_SERVER_URL)
    win.webContents.openDevTools()
  } else {
    // In production, load the built files
    win.loadFile(path.join(__dirname, '../dist/index.html'))
  }

  // Hide the menu bar
  win.setMenuBarVisibility(false)
}

// App lifecycle handlers
app.whenReady().then(() => {
  createWindow()

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) {
      createWindow()
    }
  })
})

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') {
    app.quit()
  }
})