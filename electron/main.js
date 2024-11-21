// electron/main.js
import { app, BrowserWindow } from 'electron'
import * as path from 'path'
import { fileURLToPath } from 'url'

const __filename = fileURLToPath(import.meta.url)
const __dirname = path.dirname(__filename)

function getIconPath() {
    // Determine if we're in dev or production
    const isDev = process.env.VITE_DEV_SERVER_URL ? true : false;
    
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
  const win = new BrowserWindow({
    width: 1200,
    height: 800,
    webPreferences: {
      nodeIntegration: true,
      contextIsolation: false,
    },
    icon: getIconPath(),
  })

  // In development, load from the Vite dev server
  if (process.env.VITE_DEV_SERVER_URL) {
    win.loadURL(process.env.VITE_DEV_SERVER_URL)
    win.webContents.openDevTools()
  } else {
    // In production, load the built files
    win.loadFile(path.join(__dirname, '../dist/index.html'))
  }
}

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